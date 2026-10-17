// GNOME animation is launched only by the optional Shell bridge, never as a window here.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use zbus::blocking::{Connection, connection, fdo::DBusProxy};

use crate::{
    media::{Fit, PlaybackOptions},
    project::{Media, MediaKind},
};

const SERVICE: &str = "io.github.flakesonnix.Wallbreak";
const CALL_TIMEOUT: Duration = Duration::from_secs(1);
const START_TIMEOUT: Duration = Duration::from_secs(25);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const BRIDGE_HELP: &str = "GNOME animation requires the enabled wallbreak@flakesonnix.github.io Shell bridge (GNOME 50, protocol 1). Run wallbreak gnome-extension to locate the installer; installation/activation are not automatic.";

#[zbus::proxy(
    interface = "io.github.flakesonnix.Wallbreak1",
    default_service = "io.github.flakesonnix.Wallbreak",
    default_path = "/io/github/flakesonnix/Wallbreak",
    gen_async = false
)]
trait ShellBridge {
    #[zbus(name = "Info")]
    fn info(&self) -> zbus::Result<u32>;

    #[zbus(name = "Start")]
    fn start(
        &self,
        renderer: &str,
        source: &str,
        fps: u32,
        fit: &str,
        ffmpeg: &str,
        output: &str,
    ) -> zbus::Result<String>;

    #[zbus(name = "Status")]
    fn status(&self, session: &str) -> zbus::Result<(String, String)>;

    #[zbus(name = "Stop")]
    fn stop(&self, session: &str) -> zbus::Result<()>;
}

/// Play through the optional GNOME 50 extension without changing desktop settings.
pub fn run(
    media: &Media,
    options: &PlaybackOptions,
    output: Option<&str>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    validate_options(media, options)?;
    let connection = connection::Builder::session()
        .context(BRIDGE_HELP)?
        .method_timeout(CALL_TIMEOUT)
        .build()
        .context(BRIDGE_HELP)?;
    let executable = std::env::current_exe().context("cannot locate wallbreak executable")?;
    let renderer = sibling_renderer(&executable)?;
    run_connected(connection, &renderer, media, options, output, stop)
}

fn sibling_renderer(executable: &Path) -> Result<PathBuf> {
    Ok(executable
        .parent()
        .context("wallbreak executable has no parent directory")?
        .join("wallbreak-renderer"))
}

fn validate_options(media: &Media, options: &PlaybackOptions) -> Result<()> {
    ensure!(
        media.kind != MediaKind::Image && !media.preview_only,
        "GNOME animation requires original video/scene media, not a static preview"
    );
    ensure!(
        (1..=60).contains(&options.fps),
        "FPS must be between 1 and 60"
    );
    Ok(())
}

// A supplied connection/path also allows isolated protocol tests without ambient env changes.
fn run_connected(
    connection: Connection,
    renderer: &Path,
    media: &Media,
    options: &PlaybackOptions,
    output: Option<&str>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    validate_options(media, options)?;
    ensure!(
        connection
            .method_timeout()
            .is_some_and(|timeout| timeout <= CALL_TIMEOUT),
        "GNOME bridge connection requires a bounded method timeout"
    );
    // A unique destination prevents activation/replacement without no_autostart flags, whose
    // call_with_flags path bypasses method_timeout in zbus 5.13.2.
    let destination = DBusProxy::new(&connection)
        .context(BRIDGE_HELP)?
        .get_name_owner(SERVICE.try_into()?)
        .context(BRIDGE_HELP)?;
    let proxy = ShellBridgeProxy::builder(&connection)
        .destination(destination)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .context(BRIDGE_HELP)?;
    let protocol = proxy.info().context(BRIDGE_HELP)?;
    ensure!(
        protocol == 1,
        "{BRIDGE_HELP} Bridge reported protocol {protocol}, expected 1."
    );

    ensure!(
        renderer.is_absolute(),
        "GNOME renderer path must be absolute"
    );
    let renderer = renderer
        .to_str()
        .context("GNOME renderer path is not UTF-8; D-Bus cannot represent it")?;
    ensure!(
        Path::new(renderer).is_file(),
        "GNOME animation renderer is missing: {renderer}; install the wallbreak-renderer public wrapper beside wallbreak"
    );
    let source = &media.source_path;
    ensure!(
        source.is_absolute(),
        "GNOME animation source path must be absolute"
    );
    let source = source
        .to_str()
        .context("GNOME animation source path is not UTF-8; D-Bus cannot represent it")?;
    let ffmpeg = options
        .ffmpeg
        .to_str()
        .context("FFmpeg path is not UTF-8; D-Bus cannot represent it")?;
    let fit = match options.fit {
        Fit::Cover => "cover",
        Fit::Contain => "contain",
    };
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let starting = Instant::now();
    let session = proxy
        .start(renderer, source, options.fps, fit, ffmpeg, output.unwrap_or(""))
        .context("GNOME bridge Start failed; cleanup unknown because no session identifier was received. Disconnecting this controller lets the bridge clean up its sender.")?;
    // Own even a malformed reply before validating it: Start may already have launched a renderer.
    let mut owner = Playback {
        proxy,
        session,
        owned: true,
        cleanup_deadline: None,
    };
    let result = (|| {
        let bytes = owner.session.as_bytes();
        ensure!(
            bytes.len() == 36
                && bytes.iter().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                }),
            "GNOME bridge returned an invalid session UUID"
        );
        owner.watch(options, &stop, starting)
    })();
    match (result, owner.shutdown()) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("GNOME animation cleanup also failed: {cleanup:#}")))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

struct Playback<'a> {
    // The proxy owns a connection clone, retaining the sender until cleanup (including Drop).
    proxy: ShellBridgeProxy<'a>,
    session: String,
    owned: bool,
    cleanup_deadline: Option<Instant>,
}

impl Playback<'_> {
    fn status(&self) -> Result<(State, String)> {
        let (state, detail) = self
            .proxy
            .status(&self.session)
            .context("cannot query GNOME animation Status")?;
        let state = match state.as_str() {
            "starting" => State::Starting,
            "running" => State::Running,
            "stopping" => State::Stopping,
            "stopped" => State::Stopped,
            "failed" => State::Failed,
            _ => bail!("GNOME bridge returned unknown state {state:?}: {detail}"),
        };
        Ok((state, detail))
    }

    fn watch(
        &mut self,
        options: &PlaybackOptions,
        stop: &AtomicBool,
        starting: Instant,
    ) -> Result<()> {
        let mut running: Option<Instant> = None;
        loop {
            if stop.load(Ordering::Relaxed)
                || running.is_some_and(|since| {
                    options
                        .duration
                        .is_some_and(|duration| since.elapsed() >= duration)
                })
            {
                return Ok(());
            }
            // Reserve a complete call timeout so a slow final Status cannot exceed 25 seconds.
            ensure!(
                running.is_some() || starting.elapsed() < START_TIMEOUT - CALL_TIMEOUT,
                "GNOME renderer did not reach running within 25 seconds"
            );
            let (state, detail) = self.status()?;
            match state {
                State::Starting => ensure!(
                    running.is_none(),
                    "GNOME renderer regressed from running to starting: {detail}"
                ),
                State::Running => {
                    running.get_or_insert_with(Instant::now);
                }
                State::Stopping => bail!("GNOME renderer stopped unexpectedly: {detail}"),
                State::Stopped => {
                    self.owned = false;
                    ensure!(
                        running.is_some(),
                        "GNOME renderer stopped before reaching running: {detail}"
                    );
                    return Ok(());
                }
                State::Failed => bail!("GNOME renderer failed: {detail}"),
            }
            if running.is_some_and(|since| {
                options
                    .duration
                    .is_some_and(|duration| since.elapsed() >= duration)
            }) {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        if !self.owned {
            return Ok(());
        }
        // Drop retries retain the same overall budget rather than adding another six seconds.
        let deadline = *self
            .cleanup_deadline
            .get_or_insert_with(|| Instant::now() + CLEANUP_TIMEOUT);
        ensure!(
            deadline.saturating_duration_since(Instant::now()) >= CALL_TIMEOUT,
            "GNOME animation cleanup unknown: stopped was not confirmed within the six-second cleanup budget"
        );
        let stop_error = self.proxy.stop(&self.session).err();
        let mut last_status = None;
        let result = (|| loop {
            ensure!(
                deadline.saturating_duration_since(Instant::now()) >= CALL_TIMEOUT,
                "stopped was not confirmed within the six-second cleanup budget; last status: {last_status:?}"
            );
            let (state, detail) = self.status()?;
            if state == State::Stopped {
                self.owned = false;
                return Ok(());
            }
            last_status = Some((state, detail));
            thread::sleep(POLL_INTERVAL);
        })();
        result.map_err(|error: anyhow::Error| {
            let error =
                error.context("GNOME animation cleanup unknown: bridge did not confirm stopped");
            match stop_error {
                Some(stop) => error.context(format!("GNOME bridge Stop also failed: {stop}")),
                None => error,
            }
        })
    }
}

impl Drop for Playback<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("GNOME animation cleanup failed: {error:#}");
        }
    }
}
