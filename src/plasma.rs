// Native, reversible static images. Plasma animation needs a separate integration.

use std::{
    collections::HashSet,
    env,
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, OFlags, flock};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zbus::blocking::{Connection, connection, fdo::DBusProxy};

use crate::{
    media::{Fit, MAX_FRAME_BYTES, PlaybackOptions},
    project::{Media, MediaKind},
};

const SERVICE: &str = "org.kde.plasmashell";
const CALL_TIMEOUT: Duration = Duration::from_millis(750);
const HELP: &str = "KDE Plasma image playback requires a running plasmashell with its session D-Bus scripting API enabled";

#[zbus::proxy(
    interface = "org.kde.PlasmaShell",
    default_service = "org.kde.plasmashell",
    default_path = "/PlasmaShell",
    gen_async = false
)]
trait PlasmaShell {
    #[zbus(name = "evaluateScript")]
    fn evaluate_script(&self, script: &str) -> zbus::Result<String>;
}

pub fn is_current() -> bool {
    desktop_is_plasma(env::var("XDG_CURRENT_DESKTOP").as_deref().ok())
}

fn desktop_is_plasma(desktop: Option<&str>) -> bool {
    desktop.is_some_and(|value| {
        value
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("kde") || part.eq_ignore_ascii_case("plasma"))
    })
}

/// Apply an image to the current activity's connected desktops, until stopped.
pub fn run(
    media: &Media,
    options: &PlaybackOptions,
    output: Option<&str>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    ensure!(
        media.kind == MediaKind::Image,
        "KDE Plasma currently supports static images only; select --preview for a project image. Video and animated-scene integration is not implemented."
    );
    ensure!(
        output.is_none(),
        "KDE Plasma image playback applies to all displays in the current activity; omit --output."
    );
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut image = image::ImageReader::open(&media.path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_FRAME_BYTES as u64);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    image.limits(limits);
    image
        .decode()
        .context("KDE Plasma image decoding failed; desktop unchanged")?;
    let uri = url::Url::from_file_path(&media.path)
        .map_err(|_| anyhow::anyhow!("Plasma requires an absolute local image path"))?;
    let _lock = playback_lock(Path::new(
        &env::var_os("XDG_RUNTIME_DIR").context("Plasma playback requires XDG_RUNTIME_DIR")?,
    ))?;
    let connection = connection::Builder::session()
        .context(HELP)?
        .method_timeout(CALL_TIMEOUT)
        .build()
        .context(HELP)?;
    run_connected(connection, uri.as_str(), options, stop)
}

fn playback_lock(runtime: &Path) -> Result<File> {
    let metadata = runtime
        .metadata()
        .context("cannot inspect Plasma runtime directory")?;
    let uid = rustix::process::geteuid().as_raw();
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "Plasma runtime directory must be private and owned by the current user"
    );
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(runtime.join("wallbreak-plasma.lock"))
        .context("cannot open Plasma playback lock")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == uid,
        "Plasma playback lock is not an owned regular file"
    );
    flock(&file, FlockOperation::NonBlockingLockExclusive)
        .context("another wallbreak Plasma player is already active")?;
    Ok(file)
}

fn run_connected(
    connection: Connection,
    uri: &str,
    options: &PlaybackOptions,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    ensure!(
        connection
            .method_timeout()
            .is_some_and(|timeout| timeout <= CALL_TIMEOUT),
        "Plasma connection requires a bounded method timeout"
    );
    // Bind to this shell instance, never activate or mutate its replacement.
    let destination = DBusProxy::new(&connection)?
        .get_name_owner(SERVICE.try_into()?)
        .context(HELP)?;
    let proxy = PlasmaShellProxy::builder(&connection)
        .destination(destination)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .context(HELP)?;
    let previous: Vec<Desktop> = serde_json::from_str(
        &proxy
            .evaluate_script(&script(&json!({"action": "snapshot"}))?)
            .context(HELP)?,
    )
    .context("Plasma returned an invalid desktop snapshot")?;
    validate_snapshot(&previous)?;
    let applied = Settings {
        image: uri.into(),
        fill_mode: match options.fit {
            Fit::Cover => 2,
            Fit::Contain => 1,
        },
        color: "#000000".into(),
        blur: false,
    };
    let mut background = Background {
        owner: connection
            .unique_name()
            .context("Plasma connection has no unique name")?
            .to_string(),
        proxy,
        previous,
        applied,
        owned: false,
        partial: true,
    };
    let result = (|| {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Plasma commits wallpaperPlugin at script teardown. A second invocation
        // must see the image plugin before writeConfig can safely target it.
        background.owned = true;
        background.operation("switch")?;
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        background.operation("apply")?;
        background.partial = false;
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        background.operation("verify")?;
        eprintln!(
            "KDE Plasma static image active on all displays in this activity. Stop restores settings still owned by wallbreak."
        );
        let started = Instant::now();
        while !stop.load(Ordering::Relaxed)
            && options
                .duration
                .is_none_or(|duration| started.elapsed() < duration)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();
    match (result, background.restore()) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Plasma rollback also failed: {cleanup:#}")))
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Desktop {
    id: u32,
    plugin: String,
    settings: Settings,
}

#[derive(Debug, Deserialize, Serialize)]
struct Settings {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "FillMode")]
    fill_mode: u32,
    #[serde(rename = "Color")]
    color: String,
    #[serde(rename = "Blur")]
    blur: bool,
}

fn validate_snapshot(desktops: &[Desktop]) -> Result<()> {
    ensure!(
        !desktops.is_empty(),
        "Plasma has no active desktop containments"
    );
    let mut ids = HashSet::new();
    for desktop in desktops {
        ensure!(
            desktop.id > 0 && ids.insert(desktop.id),
            "Plasma returned invalid or duplicate desktop IDs"
        );
        ensure!(
            !desktop.plugin.is_empty(),
            "Plasma desktop has no wallpaper plugin"
        );
    }
    Ok(())
}

fn script(request: &serde_json::Value) -> Result<String> {
    // JSON.parse keeps arbitrary paths/plugin names as data, including __proto__.
    let encoded = serde_json::to_string(&serde_json::to_string(request)?)?
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(format!(
        "var request = JSON.parse({encoded});\n{}",
        include_str!("plasma.js")
    ))
}

struct Background<'a> {
    owner: String,
    proxy: PlasmaShellProxy<'a>,
    previous: Vec<Desktop>,
    applied: Settings,
    owned: bool,
    partial: bool,
}

impl Background<'_> {
    fn operation(&self, action: &str) -> Result<()> {
        let request = json!({"action": action, "previous": self.previous, "applied": self.applied, "owner": self.owner, "partial": self.partial});
        let reply = self
            .proxy
            .evaluate_script(&script(&request)?)
            .with_context(|| format!("Plasma wallpaper {action} failed"))?;
        ensure!(
            reply.trim() == "null",
            "Plasma returned an invalid {action} acknowledgement"
        );
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if self.owned {
            // A timed-out script may still execute. Never queue repeated writes
            // from Drop after the controller has reported a cleanup failure.
            self.owned = false;
            self.operation("restore")?;
        }
        Ok(())
    }
}

impl Drop for Background<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("Plasma background cleanup failed: {error:#}");
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn detects_plasma_without_matching_similar_names() {
        for name in ["KDE", "kde", "Plasma", "plasma:KDE", "custom:KDE"] {
            assert!(desktop_is_plasma(Some(name)));
        }
        for name in [
            None,
            Some(""),
            Some("GNOME"),
            Some("sway"),
            Some("KDE-ish"),
            Some("plasmashell"),
        ] {
            assert!(!desktop_is_plasma(name));
        }
    }

    #[test]
    fn runtime_lock_is_exclusive_and_released() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = playback_lock(directory.path()).unwrap();
        assert!(playback_lock(directory.path()).is_err());
        drop(lock);
        assert!(playback_lock(directory.path()).is_ok());
    }

    #[test]
    fn runtime_lock_rejects_symlinks_and_public_directories() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink("/dev/null", directory.path().join("wallbreak-plasma.lock")).unwrap();
        assert!(playback_lock(directory.path()).is_err());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(playback_lock(directory.path()).is_err());
    }

    #[test]
    fn script_encodes_request_as_json_not_executable_source() {
        let request =
            json!({"action": "apply", "path": "\"; throw Error('injected'); //\n\u{2028}\u{2029}"});
        let generated = script(&request).unwrap();
        let prefix = generated.lines().next().unwrap();
        let literal = prefix
            .strip_prefix("var request = JSON.parse(")
            .unwrap()
            .strip_suffix(");")
            .unwrap();
        let json: String = serde_json::from_str(literal).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            request
        );
    }
}
