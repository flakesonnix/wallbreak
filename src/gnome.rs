//! Reversible static backgrounds for GNOME, which has no wlr-layer-shell.

use std::{
    env,
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use rustix::fs::{flock, FlockOperation, OFlags};

use crate::{
    media::{Fit, PlaybackOptions, MAX_FRAME_BYTES},
    project::{Media, MediaKind},
};

const SCHEMA: &str = "org.gnome.desktop.background";

pub fn is_current() -> bool {
    desktop_is_gnome(env::var("XDG_CURRENT_DESKTOP").as_deref().ok())
}

fn desktop_is_gnome(desktop: Option<&str>) -> bool {
    desktop.is_some_and(|value| {
        value
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("gnome"))
    })
}

/// Apply one image until cancellation or the requested duration has elapsed.
/// An explicit monitor selection is rejected: GNOME's setting is desktop-wide.
pub fn run(
    media: &Media,
    options: &PlaybackOptions,
    output: Option<&str>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if media.kind != MediaKind::Image {
        return crate::gnome_bridge::run(media, options, output, stop);
    }
    ensure!(output.is_none(), "GNOME image playback applies to all displays; omit --output instead of selecting one monitor.");
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    // Reject damaged/oversized images before changing any desktop setting.
    let mut image = image::ImageReader::open(&media.path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_FRAME_BYTES as u64);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    image.limits(limits);
    image
        .decode()
        .context("GNOME image decoding failed; desktop unchanged")?;
    let uri = url::Url::from_file_path(&media.path)
        .map_err(|_| anyhow::anyhow!("GNOME background requires an absolute local image path"))?;
    let mode = match options.fit {
        Fit::Cover => "zoom",
        Fit::Contain => "scaled",
    };
    let _lock = playback_lock()?;
    let mut background = Background::apply(uri.as_str(), mode)?;
    eprintln!("GNOME static image active on all displays. Stop restores settings still owned by wallbreak.");
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed)
        && !options
            .duration
            .is_some_and(|duration| started.elapsed() >= duration)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    background.restore()
}

fn playback_lock() -> Result<File> {
    let runtime =
        env::var_os("XDG_RUNTIME_DIR").context("GNOME playback requires XDG_RUNTIME_DIR")?;
    let runtime = Path::new(&runtime);
    let metadata = runtime
        .metadata()
        .context("cannot inspect GNOME runtime directory")?;
    let uid = rustix::process::geteuid().as_raw();
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "GNOME runtime directory must be private and owned by the current user"
    );
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(runtime.join("wallbreak-gnome.lock"))
        .context("cannot open GNOME playback lock")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == uid,
        "GNOME playback lock is not an owned regular file"
    );
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .context("another wallbreak GNOME player is already active")?;
    Ok(file)
}

struct Setting {
    key: &'static str,
    previous: String,
    applied: String,
    owned: bool,
}

struct Background {
    settings: Vec<Setting>,
}

impl Background {
    fn apply(uri: &str, mode: &str) -> Result<Self> {
        let mut settings = Vec::new();
        for (key, desired) in [
            ("picture-uri", uri),
            ("picture-uri-dark", uri),
            ("picture-options", mode),
        ] {
            let previous = gsettings(&["get", SCHEMA, key])?;
            parse_string(&previous).with_context(|| format!("invalid GNOME setting {key}"))?;
            ensure!(
                gsettings(&["writable", SCHEMA, key])? == "true",
                "GNOME setting {key} is not writable; desktop unchanged"
            );
            settings.push(Setting {
                key,
                previous,
                applied: desired.into(),
                owned: false,
            });
        }
        let mut owner = Self { settings };
        for index in 0..owner.settings.len() {
            let setting = &mut owner.settings[index];
            let write = gsettings(&[
                "set",
                SCHEMA,
                setting.key,
                &serde_json::to_string(&setting.applied)?,
            ]);
            // Even a failed subprocess could have changed its key before failing.
            setting.owned = true;
            let verified = write
                .and_then(|_| gsettings(&["get", SCHEMA, setting.key]))
                .and_then(|value| parse_string(&value))
                .and_then(|value| {
                    ensure!(
                        value == setting.applied,
                        "GNOME setting {} did not retain the requested value",
                        setting.key
                    );
                    Ok(())
                });
            if let Err(error) = verified {
                return Err(match owner.restore() {
                    Ok(()) => error,
                    Err(cleanup) => {
                        error.context(format!("GNOME rollback also failed: {cleanup:#}"))
                    }
                });
            }
        }
        Ok(owner)
    }

    fn restore(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for setting in self
            .settings
            .iter_mut()
            .rev()
            .filter(|setting| setting.owned)
        {
            let result = (|| -> Result<()> {
                let current = gsettings(&["get", SCHEMA, setting.key])?;
                if parse_string(&current)? == setting.applied {
                    gsettings(&["set", SCHEMA, setting.key, &setting.previous])?;
                    let restored = gsettings(&["get", SCHEMA, setting.key])?;
                    ensure!(
                        parse_string(&restored)? == parse_string(&setting.previous)?,
                        "GNOME setting {} could not be restored",
                        setting.key
                    );
                }
                // A different value belongs to the user or another application.
                setting.owned = false;
                Ok(())
            })();
            if let Err(error) = result {
                errors.push(format!("{}: {error:#}", setting.key));
            }
        }
        ensure!(
            errors.is_empty(),
            "GNOME restoration incomplete: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("GNOME background cleanup failed: {error:#}");
        }
    }
}

fn gsettings(args: &[&str]) -> Result<String> {
    let output = Command::new("gsettings")
        .args(args)
        .output()
        .context("cannot run gsettings; install GLib or use the Nix package")?;
    ensure!(
        output.status.success(),
        "gsettings {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout)
        .context("gsettings returned non-UTF-8 output")
        .map(|value| value.trim().to_owned())
}

/// Parse the GVariant string printed by `gsettings get`; retain its original
/// serialized form separately for restoration instead of reformatting it.
fn parse_string(serialized: &str) -> Result<String> {
    let mut chars = serialized.chars();
    let quote = chars.next().context("empty GVariant string")?;
    ensure!(
        matches!(quote, '\'' | '"'),
        "expected a quoted GVariant string"
    );
    let mut value = String::new();
    while let Some(character) = chars.next() {
        if character == quote {
            ensure!(chars.next().is_none(), "trailing GVariant string content");
            return Ok(value);
        }
        if character != '\\' {
            value.push(character);
            continue;
        }
        let escape = chars.next().context("incomplete GVariant string escape")?;
        match escape {
            '\\' | '\'' | '"' => value.push(escape),
            'a' => value.push('\u{7}'),
            'b' => value.push('\u{8}'),
            'f' => value.push('\u{c}'),
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            'v' => value.push('\u{b}'),
            'u' | 'U' => {
                let digits = if escape == 'u' { 4 } else { 8 };
                let mut number = 0;
                for _ in 0..digits {
                    number = number * 16
                        + chars
                            .next()
                            .and_then(|digit| digit.to_digit(16))
                            .context("invalid GVariant Unicode escape")?;
                }
                value.push(char::from_u32(number).context("invalid GVariant Unicode scalar")?);
            }
            _ => bail!("unsupported GVariant string escape"),
        }
    }
    bail!("unterminated GVariant string")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gnome_without_treating_unknown_desktops_as_gnome() {
        for name in ["GNOME", "gnome", "ubuntu:GNOME", "GNOME:GNOME-Classic"] {
            assert!(desktop_is_gnome(Some(name)));
        }
        for name in [None, Some(""), Some("sway"), Some("KDE"), Some("GNOME-ish")] {
            assert!(!desktop_is_gnome(name));
        }
    }

    #[test]
    fn parses_gvariant_strings_without_shell_interpretation() {
        assert_eq!(
            parse_string("'file:///some%20image.png'").unwrap(),
            "file:///some%20image.png"
        );
        assert_eq!(
            parse_string(r#""a'quote\\and\nline""#).unwrap(),
            "a'quote\\and\nline"
        );
        assert_eq!(
            parse_string("'\\u00e4\\U0001f600'").unwrap(),
            "\u{e4}\u{1f600}"
        );
        for value in [
            "",
            "false",
            "'missing",
            "'x' trailing",
            "'\\q'",
            "'\\uD800'",
        ] {
            assert!(parse_string(value).is_err(), "{value:?}");
        }
    }
}
