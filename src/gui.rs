//! Graphical control panel for wallbreak.
//!
//! The panel itself never renders wallpapers. It manages one `wallbreak play`
//! child process per user action and renders single preview frames in-process
//! through the library, without touching Wayland or the desktop.

use std::{
    io::{self, Read},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rustix::process::{Pid, Signal, kill_process};

use crate::{
    display,
    media::{Fit, MediaWorker, PlaybackOptions},
    project,
};

/// A managed `wallbreak play` child process.
///
/// Stop and Drop request graceful cleanup before escalating, for this child only.
#[derive(Default)]
pub struct Player {
    child: Option<Child>,
    description: String,
    exit_status: Option<ExitStatus>,
    diagnostics: Arc<Mutex<Diagnostics>>,
    cancel_reader: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    forced: bool,
}

const STDERR_LIMIT: usize = 32 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Diagnostics {
    bytes: Vec<u8>,
    truncated: bool,
    incomplete: bool,
    error: Option<String>,
}

impl Player {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poll without relinquishing ownership if waiting fails.
    pub fn is_running(&mut self) -> Result<bool> {
        if let Some(child) = &mut self.child {
            match child.try_wait().context("cannot poll wallpaper process")? {
                None => return Ok(true),
                Some(status) => {
                    self.exit_status = Some(status);
                    self.child = None;
                }
            }
        }
        self.finish_stderr()?;
        Ok(false)
    }

    pub fn has_pending_work(&self) -> bool {
        self.child.is_some() || self.reader.is_some()
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit_status
    }

    /// Retain the stderr tail, including explicit coverage and cleanup warnings.
    pub fn diagnostics(&self) -> String {
        let diagnostics = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = String::new();
        if diagnostics.truncated {
            text.push_str(&format!(
                "[stderr truncated; showing the last {STDERR_LIMIT} bytes]\n"
            ));
        }
        text.push_str(&String::from_utf8_lossy(&diagnostics.bytes));
        if diagnostics.incomplete {
            text.push_str("\n[stderr capture stopped before EOF]\n");
        }
        if let Some(error) = &diagnostics.error {
            text.push_str(&format!("\n[stderr capture failed: {error}]\n"));
        }
        if self.forced {
            text.push_str(
                "\n[Player required SIGKILL; wallpaper restoration may not have completed.]\n",
            );
        }
        text
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Start any program with arguments, replacing a running player.
    pub fn start(&mut self, program: &PathBuf, args: &[String], description: String) -> Result<()> {
        self.start_with_stdout(program, args, description, false)
    }

    fn start_with_stdout(
        &mut self,
        program: &PathBuf,
        args: &[String],
        description: String,
        capture: bool,
    ) -> Result<()> {
        self.stop()?;
        self.exit_status = None;
        self.forced = false;
        self.diagnostics = Arc::new(Mutex::new(Diagnostics::default()));
        self.cancel_reader = Arc::new(AtomicBool::new(false));
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(if capture {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot start {}", program.display()))?;
        self.child = Some(child);
        self.description = description;
        // Keep the child owner installed before any fallible reader setup.
        let setup = (|| -> Result<()> {
            let mut stderr = self
                .child
                .as_mut()
                .and_then(|child| child.stderr.take())
                .context("player stderr pipe is missing")?;
            rustix::io::ioctl_fionbio(&stderr, true)
                .context("cannot make player stderr nonblocking")?;
            let diagnostics = self.diagnostics.clone();
            let cancel = self.cancel_reader.clone();
            self.reader = Some(
                thread::Builder::new()
                    .name("player-stderr".into())
                    .spawn(move || {
                        let mut buffer = [0; 4096];
                        let mut cancelled_at = None;
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                let start = cancelled_at.get_or_insert_with(Instant::now);
                                // A descendant may keep writing or hold the pipe open. Do not
                                // wait for its lifetime, even while draining the final bytes.
                                if start.elapsed() >= Duration::from_millis(100) {
                                    diagnostics.lock().unwrap().incomplete = true;
                                    break;
                                }
                            }
                            match stderr.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let mut diagnostics = diagnostics.lock().unwrap();
                                    let excess =
                                        (diagnostics.bytes.len() + n).saturating_sub(STDERR_LIMIT);
                                    if excess > 0 {
                                        diagnostics.bytes.drain(..excess);
                                        diagnostics.truncated = true;
                                    }
                                    diagnostics.bytes.extend_from_slice(&buffer[..n]);
                                }
                                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                    if cancelled_at.is_some() {
                                        diagnostics.lock().unwrap().incomplete = true;
                                        break;
                                    }
                                    thread::sleep(POLL_INTERVAL);
                                }
                                Err(error) => {
                                    diagnostics.lock().unwrap().error = Some(error.to_string());
                                    break;
                                }
                            }
                        }
                    })
                    .context("cannot start player stderr reader")?,
            );
            Ok(())
        })();
        if let Err(error) = setup {
            return Err(match self.stop() {
                Ok(()) => error,
                Err(cleanup) => error.context(format!("player cleanup also failed: {cleanup:#}")),
            });
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.is_running()? {
            // An unreaped, owned Child pins this PID against reuse.
            let pid = Pid::from_child(self.child.as_ref().unwrap());
            match kill_process(pid, Signal::TERM) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => return Err(error).context("cannot send TERM to wallpaper process"),
            }
            let mut deadline = Instant::now() + TERM_GRACE;
            let mut escalated = false;
            while self.is_running()? {
                if Instant::now() >= deadline {
                    anyhow::ensure!(
                        !escalated,
                        "wallpaper process has not exited after SIGKILL; cleanup is still pending"
                    );
                    match kill_process(pid, Signal::KILL) {
                        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                        Err(error) => {
                            return Err(error).context("cannot send KILL to wallpaper process");
                        }
                    }
                    self.forced = true;
                    escalated = true;
                    deadline = Instant::now() + KILL_GRACE;
                }
                thread::sleep(POLL_INTERVAL);
            }
        }
        self.description.clear();
        Ok(())
    }

    fn finish_stderr(&mut self) -> Result<()> {
        self.cancel_reader.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| {
                self.diagnostics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error = Some("reader thread panicked".into());
                anyhow::anyhow!("player stderr reader panicked")
            })?;
        }
        Ok(())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("wallbreak: player cleanup failed: {error:#}");
            let _ = self.finish_stderr();
        }
    }
}

/// Path of the installed `wallbreak` binary next to the running GUI.
pub fn wallbreak_binary() -> Result<PathBuf> {
    let gui = std::env::current_exe().context("cannot locate the GUI executable")?;
    let dir = gui
        .parent()
        .context("GUI executable has no parent directory")?;
    let candidate = dir.join("wallbreak");
    if candidate.is_file() {
        return Ok(candidate);
    }
    anyhow::bail!(
        "wallbreak binary not found next to {}; install both binaries together",
        gui.display()
    )
}

/// Human-readable output names from the current compositor, without mapping anything.
pub fn refresh_outputs() -> Result<Vec<String>> {
    Ok(display::list_outputs()?
        .into_iter()
        .map(|output| output.name)
        .filter(|name| !name.is_empty())
        .collect())
}

#[cfg(feature = "gui")]
fn refresh_outputs_cancellable(cancel: &AtomicBool) -> Result<Vec<String>> {
    // A read-only CLI child bounds even a compositor stuck during registry setup.
    let mut process = Player::new();
    process.start_with_stdout(
        &wallbreak_binary()?,
        &["outputs".into()],
        "output discovery".into(),
        true,
    )?;
    let mut stdout = process
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .context("output discovery stdout is missing")?;
    rustix::io::ioctl_fionbio(&stdout, true)?;
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        anyhow::ensure!(
            !cancel.load(Ordering::Relaxed),
            "output discovery cancelled"
        );
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(5),
            "output discovery timed out after 5 seconds"
        );
        match stdout.read(&mut buffer) {
            Ok(0) => {
                if !process.is_running()? {
                    break;
                }
            }
            Ok(count) => {
                anyhow::ensure!(
                    bytes.len() + count <= 1024 * 1024,
                    "output discovery exceeds 1 MiB"
                );
                bytes.extend_from_slice(&buffer[..count]);
                continue;
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("cannot read output discovery"),
        }
        thread::sleep(POLL_INTERVAL);
    }
    anyhow::ensure!(
        process.exit_status().is_some_and(|status| status.success()),
        "output discovery failed: {}",
        process.diagnostics()
    );
    #[derive(serde::Deserialize)]
    struct Output {
        name: String,
    }
    let outputs: Vec<Output> =
        serde_json::from_slice(&bytes).context("invalid output discovery response")?;
    Ok(outputs
        .into_iter()
        .map(|output| output.name)
        .filter(|name| !name.is_empty())
        .collect())
}

/// Render one preview frame in-process. Images yield their fitted frame;
/// scenes and videos yield one decoded frame. Nothing is displayed on the desktop.
pub fn render_preview(
    path: &std::path::Path,
    preview: bool,
    fit: Fit,
    ffmpeg: &std::path::Path,
) -> Result<(Vec<u8>, u32, u32)> {
    render_preview_cancellable(path, preview, fit, ffmpeg, &AtomicBool::new(false))
}

fn render_preview_cancellable(
    path: &std::path::Path,
    preview: bool,
    fit: Fit,
    ffmpeg: &std::path::Path,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, u32, u32)> {
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "preview cancelled");
    let media = project::resolve(path, preview)?;
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "preview cancelled");
    let (width, height) = (320, 180);
    let options = PlaybackOptions {
        fps: 30,
        fit,
        ffmpeg: ffmpeg.to_path_buf(),
        duration: None,
    };
    let mut worker = MediaWorker::start(&media, width, height, &options)?;
    let start = Instant::now();
    let frame = loop {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "preview cancelled");
        if let Some(frame) = worker.take_frame()? {
            break frame.into_bgra()?;
        }
        anyhow::ensure!(
            start.elapsed() <= Duration::from_secs(15),
            "decoder produced no preview frame within 15 seconds"
        );
        thread::sleep(Duration::from_millis(5));
    };
    worker.shutdown()?;
    Ok((frame, width, height))
}

/// Convert one BGRA frame to RGBA bytes for egui.
pub fn bgra_to_rgba(frame: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(frame.len());
    for pixel in frame.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_rgba_swaps_red_and_blue() {
        assert_eq!(bgra_to_rgba(&[10, 20, 30, 255]), vec![30, 20, 10, 255]);
        assert_eq!(bgra_to_rgba(&[]), Vec::<u8>::new());
    }

    #[test]
    fn wait_failure_retains_the_child_and_prevents_replacement() {
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = Pid::from_child(&child);
        // Deliberately reap outside Child to provoke a real ECHILD wait failure.
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()).unwrap();
        let mut player = Player::new();
        player.child = Some(child);
        assert!(
            player
                .is_running()
                .unwrap_err()
                .to_string()
                .contains("poll")
        );
        assert!(player.has_pending_work());
        assert!(player.stop().is_err());
        assert_eq!(Pid::from_child(player.child.as_ref().unwrap()), pid);
        assert!(
            player
                .start(&PathBuf::from("must-not-run"), &[], "new".into())
                .is_err()
        );
        assert_eq!(Pid::from_child(player.child.as_ref().unwrap()), pid);
        assert!(player.exit_status().is_none());
    }

    #[test]
    fn render_preview_decodes_an_image_without_wayland() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.png");
        image::RgbaImage::from_pixel(16, 16, image::Rgba([17, 53, 89, 255]))
            .save(&path)
            .unwrap();
        let (frame, width, height) = render_preview(
            &path,
            false,
            Fit::Cover,
            std::path::Path::new("missing-ffmpeg"),
        )
        .unwrap();
        assert_eq!((width, height), (320, 180));
        assert_eq!(frame.len(), 320 * 180 * 4);
        // Opaque output with the source color represented.
        assert!(frame.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
        assert!(frame.as_chunks::<4>().0.iter().any(|pixel| pixel[2] > 0));
    }

    #[test]
    fn render_preview_rejects_unsupported_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.json");
        std::fs::write(&path, r#"{"type":"scene"}"#).unwrap();
        assert!(render_preview(&path, false, Fit::Cover, std::path::Path::new("ffmpeg")).is_err());
    }
}

#[cfg(feature = "gui")]
pub mod panel {
    use super::*;
    use crate::{
        catalog,
        library::{self, LibrarySettings, PlaybackProfile, SortOrder},
        steam::ProjectEntry,
    };
    use eframe::egui;
    use std::{collections::HashMap, hash::BuildHasher, path::Path};

    struct Job<T> {
        cancel: Arc<AtomicBool>,
        thread: Option<JoinHandle<Result<T>>>,
    }

    impl<T: Send + 'static> Job<T> {
        fn start(
            name: &str,
            work: impl FnOnce(&AtomicBool) -> Result<T> + Send + 'static,
        ) -> Result<Self> {
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            let thread = thread::Builder::new()
                .name(name.into())
                .spawn(move || work(&signal))
                .with_context(|| format!("cannot start {name}"))?;
            Ok(Self {
                cancel,
                thread: Some(thread),
            })
        }

        fn take_finished(&mut self) -> Option<Result<T>> {
            if !self.thread.as_ref()?.is_finished() {
                return None;
            }
            Some(
                self.thread
                    .take()
                    .unwrap()
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("background task panicked"))),
            )
        }
    }

    impl<T> Drop for Job<T> {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    enum CatalogEvent {
        Entries(catalog::ScanResult),
        Thumbnail(PathBuf, egui::ColorImage),
        Notice(String),
    }

    #[derive(Clone, PartialEq)]
    struct PreviewRequest {
        path: PathBuf,
        preview: bool,
        fit: Fit,
        ffmpeg: PathBuf,
    }

    pub struct Panel {
        pub media_path: String,
        pub preview_flag: bool,
        pub projects: Vec<ProjectEntry>,
        pub project: String,
        pub search: String,
        pub kind_filter: String,
        pub supported_only: bool,
        pub thumbnails: HashMap<PathBuf, egui::ColorImage>,
        pub thumb_textures: HashMap<PathBuf, egui::TextureHandle>,
        pub outputs: Vec<String>,
        pub output: String,
        pub fps: u32,
        pub fit: Fit,
        pub ffmpeg: String,
        pub status: String,
        pub inspect_json: String,
        pub preview_image: Option<egui::ColorImage>,
        pub preview_texture: Option<egui::TextureHandle>,
        pub player: Player,
        pub library: LibrarySettings,
        folder_input: String,
        tag_input: String,
        profile_name: String,
        settings_path: Option<PathBuf>,
        saved_settings: Option<LibrarySettings>,
        settings_error: Option<String>,
        catalog_warnings: Vec<String>,
        scan: Option<Job<()>>,
        catalog_events: Option<std::sync::mpsc::Receiver<CatalogEvent>>,
        scan_again: bool,
        output_scan: Option<Job<Vec<String>>>,
        preview_job: Option<Job<(Vec<u8>, u32, u32)>>,
        preview_selection: Option<PreviewRequest>,
        selection: Option<PathBuf>,
        playlist_deadline: Option<Instant>,
        playlist_current: Option<PathBuf>,
        shuffle_bag: Vec<PathBuf>,
        shuffle_random: std::collections::hash_map::RandomState,
        shuffle_counter: u64,
        playlist_view: bool,
        styled: bool,
    }

    impl Default for Panel {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Panel {
        pub fn new() -> Self {
            Self {
                media_path: String::new(),
                preview_flag: false,
                projects: Vec::new(),
                project: String::new(),
                search: String::new(),
                kind_filter: "All".to_string(),
                supported_only: false,
                thumbnails: std::collections::HashMap::new(),
                thumb_textures: std::collections::HashMap::new(),
                outputs: Vec::new(),
                output: String::new(),
                fps: 30,
                fit: Fit::Cover,
                ffmpeg: "ffmpeg".to_string(),
                status: "Select media, then Inspect or Play.".to_string(),
                inspect_json: String::new(),
                preview_image: None,
                preview_texture: None,
                player: Player::new(),
                library: LibrarySettings::default(),
                folder_input: String::new(),
                tag_input: String::new(),
                profile_name: String::new(),
                settings_path: None,
                saved_settings: None,
                settings_error: None,
                catalog_warnings: Vec::new(),
                scan: None,
                catalog_events: None,
                scan_again: false,
                output_scan: None,
                preview_job: None,
                preview_selection: None,
                selection: None,
                playlist_deadline: None,
                playlist_current: None,
                shuffle_bag: Vec::new(),
                shuffle_random: Default::default(),
                shuffle_counter: 0,
                playlist_view: false,
                styled: false,
            }
        }

        fn current_profile(&self) -> PlaybackProfile {
            let path = if self.media_path.is_empty() {
                PathBuf::new()
            } else {
                self.selection.clone().unwrap_or_else(|| {
                    std::path::absolute(&self.media_path)
                        .unwrap_or_else(|_| PathBuf::from(&self.media_path))
                })
            };
            PlaybackProfile {
                path,
                preview: self.preview_flag,
                output: self.output.clone(),
                fps: self.fps,
                contain: self.fit == Fit::Contain,
                ffmpeg: PathBuf::from(&self.ffmpeg),
            }
        }

        fn apply_profile(&mut self, profile: &PlaybackProfile) {
            self.media_path = profile.path.to_string_lossy().into_owned();
            self.preview_flag = profile.preview;
            self.output = profile.output.clone();
            self.fps = profile.fps;
            self.fit = if profile.contain {
                Fit::Contain
            } else {
                Fit::Cover
            };
            self.ffmpeg = profile.ffmpeg.to_string_lossy().into_owned();
            self.selection_changed();
        }

        pub fn load_settings(&mut self) {
            match library::settings_path().and_then(|path| {
                let settings = library::load(&path)?;
                Ok((path, settings))
            }) {
                Ok((path, settings)) => {
                    self.apply_profile(&settings.playback);
                    self.library = settings.clone();
                    self.saved_settings = Some(settings);
                    self.settings_path = Some(path);
                    self.settings_error = None;
                    self.selection_changed();
                }
                Err(error) => self.settings_error = Some(format!("Settings not loaded: {error:#}")),
            }
        }

        fn save_settings(&mut self) {
            self.library.playback = self.current_profile();
            let result = (|| -> Result<()> {
                let path = match &self.settings_path {
                    Some(path) => path.clone(),
                    None => library::settings_path()?,
                };
                library::save(&path, &self.library)?;
                self.settings_path = Some(path);
                self.saved_settings = Some(self.library.clone());
                Ok(())
            })();
            match result {
                Ok(()) => {
                    self.settings_error = None;
                    self.status =
                        "Settings saved. Previous settings are retained in library.json.bak."
                            .into();
                }
                Err(error) => self.settings_error = Some(format!("Settings not saved: {error:#}")),
            }
        }

        fn selection_changed(&mut self) {
            self.selection = if self.media_path.is_empty() {
                None
            } else {
                Path::new(&self.media_path)
                    .canonicalize()
                    .ok()
                    .map(|path| {
                        if path.file_name().is_some_and(|name| name == "project.json") {
                            path.parent().unwrap().to_path_buf()
                        } else {
                            path
                        }
                    })
                    .filter(|path| path.to_str().is_some())
            };
            self.tag_input = self
                .library
                .tags
                .get(
                    self.selection
                        .as_deref()
                        .unwrap_or(Path::new(&self.media_path)),
                )
                .map(|tags| tags.join(", "))
                .unwrap_or_default();
            self.inspect_json.clear();
            self.invalidate_preview();
        }

        fn invalidate_preview(&mut self) {
            self.preview_texture = None;
            self.preview_image = None;
            if let Some(job) = &self.preview_job {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }

        fn selected_output(&self) -> Option<String> {
            if self.output.is_empty() {
                None
            } else {
                Some(self.output.clone())
            }
        }

        pub fn refresh_projects(&mut self) {
            if let Some(job) = &self.scan {
                job.cancel.store(true, Ordering::Relaxed);
                self.scan_again = true;
                return;
            }
            let folders = self.library.folders.clone();
            let (sender, receiver) = std::sync::mpsc::sync_channel(4);
            match Job::start("wallbreak-library", move |cancel| {
                let publish = |mut event| -> Result<()> {
                    loop {
                        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "library scan cancelled");
                        match sender.try_send(event) {
                            Ok(()) => return Ok(()),
                            Err(std::sync::mpsc::TrySendError::Full(pending)) => {
                                event = pending;
                                thread::sleep(POLL_INTERVAL);
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                anyhow::bail!("library view closed")
                            }
                        }
                    }
                };
                let catalog = catalog::scan(&crate::steam::default_roots(), &folders, cancel)?;
                let previews: Vec<_> = catalog
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .preview
                            .as_ref()
                            .map(|preview| (entry.path.clone(), preview.clone()))
                    })
                    .collect();
                publish(CatalogEvent::Entries(catalog))?;
                let mut retained = 0;
                for (path, preview) in previews {
                    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "library scan cancelled");
                    if retained + 192 * 192 * 4 > 32 * 1024 * 1024 {
                        publish(CatalogEvent::Notice("Thumbnail cache reached its 32 MiB budget; remaining entries are shown without thumbnails.".into()))?;
                        break;
                    }
                    let image = (|| -> Result<image::RgbaImage> {
                        let mut reader =
                            image::ImageReader::open(&preview)?.with_guessed_format()?;
                        let mut limits = image::Limits::default();
                        limits.max_alloc = Some(crate::media::MAX_FRAME_BYTES as u64);
                        limits.max_image_width = Some(16_384);
                        limits.max_image_height = Some(16_384);
                        reader.limits(limits);
                        Ok(reader.decode()?.thumbnail(192, 192).into_rgba8())
                    })();
                    if let Ok(thumb) = image {
                        let size = [thumb.width() as usize, thumb.height() as usize];
                        retained += size[0] * size[1] * 4;
                        publish(CatalogEvent::Thumbnail(
                            path,
                            egui::ColorImage::from_rgba_unmultiplied(size, &thumb.into_raw()),
                        ))?;
                    }
                }
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "library scan cancelled");
                Ok(())
            }) {
                Ok(job) => {
                    self.scan = Some(job);
                    self.catalog_events = Some(receiver);
                    self.status = "Loading library and thumbnails...".into();
                }
                Err(error) => self.status = format!("Library scan failed: {error:#}"),
            }
        }

        fn select_project(&mut self, entry: &ProjectEntry) -> bool {
            let Some(path) = entry.path.to_str() else {
                self.status = "The GUI requires UTF-8 paths. This wallpaper can be selected through the CLI without changing its filename.".into();
                return false;
            };
            self.project = entry.id.clone();
            if self.selection.as_ref() == Some(&entry.path) {
                return true;
            }
            self.media_path = path.to_owned();
            self.preview_flag = false;
            self.selection_changed();
            self.status = entry.support_note.clone();
            self.selection.is_some()
        }

        fn apply_project(&mut self, entry: &ProjectEntry, preview: bool) {
            if !preview && !entry.supported {
                self.status = format!("Cannot apply original wallpaper: {}", entry.support_note);
                return;
            }
            if self.select_project(entry) {
                if self.preview_flag != preview {
                    self.preview_flag = preview;
                    self.invalidate_preview();
                }
                self.playlist_deadline = None;
                self.play();
            }
        }

        fn project_interaction(&mut self, entry: &ProjectEntry, response: &egui::Response) {
            if response.double_clicked() {
                self.apply_project(entry, false);
            } else if response.clicked() || response.secondary_clicked() {
                self.select_project(entry);
            }
            response.context_menu(|ui| {
                ui.set_max_width(280.0);
                ui.add(egui::Label::new(egui::RichText::new(&entry.title).strong()).wrap());
                ui.separator();
                let utf8 = entry.path.to_str().is_some();
                let static_preview = entry.preview.is_some()
                    && (entry.path.is_dir() || project::file_kind(&entry.path) == Some(crate::project::MediaKind::Scene));
                for (label, preview, available) in [
                    ("Apply Wallpaper", false, entry.supported),
                    ("Apply Static Preview", true, static_preview),
                ] {
                    if ui.add_enabled(utf8 && available, egui::Button::new(label))
                        .on_disabled_hover_text(&entry.support_note).clicked() {
                        self.apply_project(entry, preview);
                        ui.close_menu();
                    }
                }
                for (label, preview, available) in [
                    ("Render Wallpaper Preview", false, entry.supported),
                    ("Render Static Preview", true, static_preview),
                ] {
                    if ui.add_enabled(utf8 && available && self.preview_job.is_none(), egui::Button::new(label))
                        .on_disabled_hover_text("Finish or cancel the current preview first; unsupported originals need a declared static preview.")
                        .clicked() {
                        if self.select_project(entry) {
                            self.preview_flag = preview;
                            self.invalidate_preview();
                            self.preview();
                        }
                        ui.close_menu();
                    }
                }
                if ui.add_enabled(utf8, egui::Button::new("Inspect")).clicked() {
                    if self.select_project(entry) { self.inspect(); }
                    ui.close_menu();
                }
                ui.separator();
                let favorite = self.library.favorites.contains(&entry.path);
                if ui.add_enabled(utf8, egui::Button::new(if favorite { "Remove from Favorites" } else { "Add to Favorites" })).clicked() {
                    self.toggle_favorite(&entry.path);
                    ui.close_menu();
                }
                let queued = self.library.playlist.contains(&entry.path);
                if ui.add_enabled(utf8 && (queued || entry.supported), egui::Button::new(
                    if queued { "Remove from Playlist" } else { "Add Original to Playlist" })).clicked() {
                    if queued {
                        self.library.playlist.retain(|path| *path != entry.path);
                        self.shuffle_bag.clear();
                        if self.library.playlist.is_empty() { self.playlist_deadline = None; }
                    } else if self.select_project(entry) {
                        if self.preview_flag {
                            self.preview_flag = false;
                            self.invalidate_preview();
                        }
                        self.add_to_playlist();
                    }
                    ui.close_menu();
                }
                if ui.add_enabled(utf8, egui::Button::new("Copy Path")).clicked() {
                    if let Some(path) = entry.path.to_str() {
                        ui.output_mut(|output| output.copied_text = path.to_owned());
                    }
                    ui.close_menu();
                }
            });
        }

        fn visible_projects(&self) -> Vec<ProjectEntry> {
            let query = self.search.to_lowercase();
            let mut entries: Vec<_> = self
                .projects
                .iter()
                .filter(|entry| {
                    (self.kind_filter == "All" || entry.kind == self.kind_filter)
                        && (!self.supported_only || entry.supported)
                        && (!self.library.favorites_only
                            || self.library.favorites.contains(&entry.path))
                        && query.split_whitespace().all(|term| {
                            entry.title.to_lowercase().contains(term)
                                || entry.kind.to_lowercase().contains(term)
                                || entry.id.to_lowercase().contains(term)
                                || self.library.tags.get(&entry.path).is_some_and(|tags| {
                                    tags.iter().any(|tag| tag.to_lowercase().contains(term))
                                })
                        })
                })
                .cloned()
                .collect();
            if self.playlist_view {
                entries.retain(|entry| self.library.playlist.contains(&entry.path));
                entries.sort_by_key(|entry| {
                    self.library
                        .playlist
                        .iter()
                        .position(|path| *path == entry.path)
                });
                return entries;
            }
            entries.sort_by(|a, b| {
                let order = match self.library.sort {
                    SortOrder::Title => std::cmp::Ordering::Equal,
                    SortOrder::Kind => a.kind.cmp(&b.kind),
                    SortOrder::Recent => self
                        .library
                        .last_used
                        .get(&b.path)
                        .cmp(&self.library.last_used.get(&a.path)),
                    SortOrder::Favorites => self
                        .library
                        .favorites
                        .contains(&b.path)
                        .cmp(&self.library.favorites.contains(&a.path)),
                };
                order
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                    .then_with(|| a.path.cmp(&b.path))
            });
            entries
        }

        fn toggle_favorite(&mut self, path: &Path) {
            if path.to_str().is_none() {
                self.status = "Favorites require UTF-8 paths; this filename is not changed.".into();
                return;
            }
            if !self.library.favorites.remove(path) {
                self.library.favorites.insert(path.to_path_buf());
            }
        }

        fn thumbnail(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
            if !self.thumb_textures.contains_key(path) {
                if let Some(image) = self.thumbnails.remove(path) {
                    self.thumb_textures.insert(
                        path.to_path_buf(),
                        ctx.load_texture(
                            format!("wallbreak-project-{}", path.display()),
                            image,
                            Default::default(),
                        ),
                    );
                }
            }
            self.thumb_textures.get(path).cloned()
        }

        /// One gallery card with the same interactions as list rows.
        fn project_card(
            &mut self,
            ui: &mut egui::Ui,
            ctx: &egui::Context,
            entry: &ProjectEntry,
            card_width: f32,
        ) {
            let selected = self.selection.as_ref() == Some(&entry.path);
            let frame = egui::Frame::group(ui.style())
                .inner_margin(5.0)
                .rounding(3.0)
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if selected {
                        egui::Color32::from_rgb(66, 166, 238)
                    } else {
                        egui::Color32::from_rgb(46, 58, 72)
                    },
                ))
                .fill(if selected {
                    egui::Color32::from_rgb(31, 63, 88)
                } else {
                    ui.style().visuals.faint_bg_color
                });
            let content_width = (card_width - frame.total_margin().sum().x).max(1.0);
            frame.show(ui, |ui| {
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(content_width);
                    let target = egui::vec2(content_width, content_width * 10.0 / 16.0);
                    let image_response = if let Some(texture) = self.thumbnail(ctx, &entry.path) {
                        let aspect = texture.size_vec2();
                        // Fixed 16:10 viewport with center crop, like WE cards.
                        let scale =
                            (target.x / aspect.x.max(1.0)).max(target.y / aspect.y.max(1.0));
                        let shown = egui::vec2(aspect.x * scale, aspect.y * scale);
                        let uv = egui::Rect::from_min_max(
                            egui::pos2(
                                ((shown.x - target.x) / 2.0 / shown.x).max(0.0),
                                ((shown.y - target.y) / 2.0 / shown.y).max(0.0),
                            ),
                            egui::pos2(
                                1.0 - ((shown.x - target.x) / 2.0 / shown.x).max(0.0),
                                1.0 - ((shown.y - target.y) / 2.0 / shown.y).max(0.0),
                            ),
                        );
                        ui.add(
                            egui::ImageButton::new((texture.id(), target))
                                .uv(uv)
                                .selected(selected)
                                .frame(false),
                        )
                    } else {
                        let (rect, response) = ui.allocate_exact_size(target, egui::Sense::click());
                        ui.painter()
                            .rect_filled(rect, 2.0, egui::Color32::from_rgb(23, 32, 43));
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            entry.kind.to_uppercase(),
                            egui::FontId::proportional(13.0),
                            egui::Color32::from_rgb(110, 130, 150),
                        );
                        response
                    };
                    self.project_interaction(entry, &image_response);
                    let title_response = ui
                        .add(
                            egui::Label::new(egui::RichText::new(&entry.title).small().strong())
                                .truncate()
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(format!(
                            "{}\n{}\n{}",
                            entry.title,
                            entry.path.display(),
                            entry.support_note
                        ));
                    self.project_interaction(entry, &title_response);
                    ui.horizontal_wrapped(|ui| {
                        let favorite = self.library.favorites.contains(&entry.path);
                        if ui
                            .selectable_label(favorite, "Fav")
                            .on_hover_text("Toggle favorite")
                            .clicked()
                        {
                            self.toggle_favorite(&entry.path);
                        }
                        ui.small(&entry.kind);
                        if !entry.supported {
                            ui.small(egui::RichText::new("unsupported").weak());
                        }
                    });
                });
            });
        }

        pub fn refresh(&mut self) {
            self.refresh_projects();
            self.refresh_output_list();
        }

        fn refresh_output_list(&mut self) {
            if self.output_scan.is_none() {
                match Job::start("wallbreak-outputs", refresh_outputs_cancellable) {
                    Ok(job) => self.output_scan = Some(job),
                    Err(error) => self.status = format!("Outputs failed: {error:#}"),
                }
            }
        }

        fn inspect(&mut self) {
            let path = PathBuf::from(&self.media_path);
            match project::resolve(&path, self.preview_flag) {
                Ok(media) => {
                    self.inspect_json = serde_json::to_string_pretty(&media).unwrap_or_default();
                    let kinds = media
                        .limitations
                        .iter()
                        .map(|line| format!("Note: {line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.status = if kinds.is_empty() {
                        "Media resolved.".to_string()
                    } else {
                        kinds
                    };
                }
                Err(error) => {
                    self.inspect_json.clear();
                    self.status = format!("Resolve failed: {error:#}");
                }
            }
        }

        fn preview(&mut self) {
            if self.preview_job.is_some() {
                return;
            }
            let selection = self.preview_request();
            let request = selection.clone();
            match Job::start("wallbreak-preview", move |cancel| {
                render_preview_cancellable(
                    &request.path,
                    request.preview,
                    request.fit,
                    &request.ffmpeg,
                    cancel,
                )
            }) {
                Ok(job) => {
                    self.preview_job = Some(job);
                    self.preview_selection = Some(selection);
                    self.status = "Rendering preview...".into();
                }
                Err(error) => self.status = format!("Preview failed: {error:#}"),
            }
        }

        fn preview_request(&self) -> PreviewRequest {
            PreviewRequest {
                path: self.current_profile().path,
                preview: self.preview_flag,
                fit: self.fit,
                ffmpeg: PathBuf::from(&self.ffmpeg),
            }
        }

        fn poll_jobs(&mut self, ctx: &egui::Context) {
            if let Some(receiver) = &self.catalog_events {
                for event in receiver.try_iter().take(8) {
                    if self.scan_again {
                        continue;
                    }
                    match event {
                        CatalogEvent::Entries(catalog) => {
                            self.projects = catalog.entries;
                            self.catalog_warnings = catalog.warnings;
                            self.thumbnails.clear();
                            self.thumb_textures.clear();
                            self.status = format!(
                                "{} wallpapers loaded; preparing thumbnails...",
                                self.projects.len()
                            );
                        }
                        CatalogEvent::Thumbnail(path, image) => {
                            self.thumbnails.insert(path, image);
                        }
                        CatalogEvent::Notice(notice) => self.catalog_warnings.push(notice),
                    }
                }
            }
            if self.scan.is_none() {
                self.catalog_events = None;
            }
            if let Some(result) = self.scan.as_mut().and_then(Job::take_finished) {
                self.scan = None;
                ctx.request_repaint();
                // The producer can finish after publishing its last frames. Drain
                // those on the next repaint before releasing the receiver.
                if self.scan_again {
                    self.scan_again = false;
                    self.refresh_projects();
                } else {
                    match result {
                        Ok(()) => {
                            self.status = format!(
                                "Library loaded: {} entries, {} warnings.",
                                self.projects.len(),
                                self.catalog_warnings.len()
                            );
                        }
                        Err(error) => self.status = format!("Library scan: {error:#}"),
                    }
                }
            }
            if let Some(result) = self.output_scan.as_mut().and_then(Job::take_finished) {
                self.output_scan = None;
                match result {
                    Ok(outputs) => self.outputs = outputs,
                    Err(error) => self.status = format!("Outputs failed: {error:#}"),
                }
            }
            if let Some(result) = self.preview_job.as_mut().and_then(Job::take_finished) {
                self.preview_job = None;
                if self.preview_selection.take().as_ref() == Some(&self.preview_request()) {
                    match result {
                        Ok((frame, width, height)) => {
                            let image = egui::ColorImage::from_rgba_unmultiplied(
                                [width as usize, height as usize],
                                &bgra_to_rgba(&frame),
                            );
                            self.preview_texture = Some(ctx.load_texture(
                                "wallbreak-preview",
                                image.clone(),
                                Default::default(),
                            ));
                            self.preview_image = Some(image);
                            self.status = format!("Preview: {width}x{height}.");
                        }
                        Err(error) => self.status = format!("Preview: {error:#}"),
                    }
                } else {
                    self.status = "Selection changed; the previous preview was discarded.".into();
                }
            }
        }

        fn play(&mut self) -> bool {
            let binary = match wallbreak_binary() {
                Ok(binary) => binary,
                Err(error) => {
                    self.status = format!("Play failed: {error:#}");
                    return false;
                }
            };
            let mut args = vec![
                "play".to_string(),
                self.media_path.clone(),
                "--fps".to_string(),
                self.fps.to_string(),
                "--fit".to_string(),
                match self.fit {
                    Fit::Cover => "cover".to_string(),
                    Fit::Contain => "contain".to_string(),
                },
                "--ffmpeg".to_string(),
                self.ffmpeg.clone(),
            ];
            if self.preview_flag {
                args.push("--preview".to_string());
            }
            if let Some(output) = self.selected_output() {
                args.push("--output".to_string());
                args.push(output.clone());
            }
            let description = format!(
                "{} on {}",
                self.media_path,
                self.selected_output()
                    .unwrap_or_else(|| "default output".to_string())
            );
            match self.player.start(&binary, &args, description) {
                Ok(()) => {
                    self.library.mark_used(&self.current_profile().path);
                    self.status = "Starting player. Stop requests wallpaper cleanup.".to_string();
                    true
                }
                Err(error) => {
                    self.status = format!("Play failed: {error:#}");
                    false
                }
            }
        }

        fn stop(&mut self) {
            self.playlist_deadline = None;
            match self.player.stop() {
                Ok(()) => self.status = "Stopped.".to_string(),
                Err(error) => self.status = format!("Stop failed: {error:#}"),
            }
        }

        fn advance_playlist(&mut self) {
            if self.library.playlist.is_empty() {
                self.playlist_deadline = None;
                self.status = "Add wallpapers to the playlist first.".into();
                return;
            }
            let path = if self.library.shuffle {
                if self.shuffle_bag.is_empty() {
                    self.shuffle_bag = self.library.playlist.clone();
                }
                self.shuffle_counter = self.shuffle_counter.wrapping_add(1);
                let mut index = self.shuffle_random.hash_one(self.shuffle_counter) as usize
                    % self.shuffle_bag.len();
                if self.shuffle_bag.len() > 1
                    && self.playlist_current.as_ref() == self.shuffle_bag.get(index)
                {
                    index = (index + 1) % self.shuffle_bag.len();
                }
                self.shuffle_bag.swap_remove(index)
            } else {
                let index = self
                    .playlist_current
                    .as_ref()
                    .and_then(|current| {
                        self.library
                            .playlist
                            .iter()
                            .position(|path| path == current)
                    })
                    .map_or(0, |index| (index + 1) % self.library.playlist.len());
                self.library.playlist[index].clone()
            };
            self.media_path = path.to_string_lossy().into_owned();
            self.preview_flag = false;
            self.selection_changed();
            self.playlist_current = Some(path);
            self.playlist_deadline = if self.play() {
                Some(Instant::now() + Duration::from_secs(self.library.interval_seconds))
            } else {
                None
            };
        }

        fn add_to_playlist(&mut self) {
            if self.media_path.is_empty() || self.preview_flag {
                self.status = "Select original media to add to the playlist; project previews are not queued.".into();
                return;
            }
            let Some(path) = self.selection.clone() else {
                self.status = "Select an existing wallpaper with a UTF-8 path before adding it to the playlist.".into();
                return;
            };
            if self
                .projects
                .iter()
                .any(|entry| entry.path == path && !entry.supported)
            {
                self.status = "This project cannot be played as original media.".into();
                return;
            }
            if !self.library.playlist.contains(&path) {
                self.library.playlist.push(path);
                self.shuffle_bag.clear();
            }
        }

        fn add_folder(&mut self) {
            match Path::new(&self.folder_input).canonicalize() {
                Ok(path) if path.is_dir() && path.to_str().is_some() => {
                    if !self.library.folders.contains(&path) {
                        self.library.folders.push(path);
                    }
                    self.folder_input.clear();
                    self.refresh_projects();
                }
                Ok(_) => self.status = "Choose a directory with a UTF-8 path.".into(),
                Err(error) => self.status = format!("Cannot add folder: {error}"),
            }
        }
        fn library_controls(&mut self, ui: &mut egui::Ui) {
            ui.small(
                egui::RichText::new("FILTERS")
                    .strong()
                    .color(egui::Color32::from_rgb(132, 153, 175)),
            );
            ui.label("Type:");
            egui::ComboBox::from_id_source("kind-filter")
                .selected_text(&self.kind_filter)
                .show_ui(ui, |ui| {
                    for kind in ["All", "image", "scene", "video", "web", "unknown"] {
                        ui.selectable_value(&mut self.kind_filter, kind.to_string(), kind);
                    }
                });
            ui.checkbox(&mut self.supported_only, "Supported only");
            ui.checkbox(&mut self.library.favorites_only, "Favorites only");
            ui.label("Sort:");
            egui::ComboBox::from_id_source("library-sort")
                .selected_text(match self.library.sort {
                    SortOrder::Title => "Title",
                    SortOrder::Kind => "Type",
                    SortOrder::Recent => "Recently started",
                    SortOrder::Favorites => "Favorites first",
                })
                .show_ui(ui, |ui| {
                    for (value, label) in [
                        (SortOrder::Title, "Title"),
                        (SortOrder::Kind, "Type"),
                        (SortOrder::Recent, "Recently started"),
                        (SortOrder::Favorites, "Favorites first"),
                    ] {
                        ui.selectable_value(&mut self.library.sort, value, label);
                    }
                });
            if !self.library.list_view {
                ui.add(
                    egui::Slider::new(&mut self.library.card_size, 120.0..=360.0)
                        .show_value(false)
                        .text("Size"),
                );
            }
            egui::CollapsingHeader::new("Media folders").show(ui, |ui| {
                ui.label(
                    "Direct images/videos and immediate project folders; no recursive scanning.",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.folder_input)
                        .hint_text("/path/to/wallpapers")
                        .desired_width(ui.available_width()),
                );
                if ui.button("Add folder").clicked() {
                    self.add_folder();
                }
                let mut remove = None;
                for (index, path) in self.library.folders.iter().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                        ui.label(path.display().to_string());
                    });
                }
                if let Some(index) = remove {
                    self.library.folders.remove(index);
                    self.refresh_projects();
                }
            });
            ui.separator();
        }

        fn playback_controls(&mut self, ui: &mut egui::Ui) {
            ui.small(
                egui::RichText::new("WALLPAPER PROPERTIES")
                    .strong()
                    .color(egui::Color32::from_rgb(132, 153, 175)),
            );
            ui.label("Output:");
            egui::ComboBox::from_id_source("output")
                .selected_text(if self.output.is_empty() {
                    "default".to_string()
                } else {
                    self.output.clone()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.output, String::new(), "default");
                    for output in self.outputs.clone() {
                        ui.selectable_value(&mut self.output, output.clone(), output);
                    }
                });
            if ui.button("Refresh outputs").clicked() {
                self.refresh_output_list();
            }
            ui.label("FPS:");
            ui.add(egui::Slider::new(&mut self.fps, 1..=60));
            ui.label("Fit:");
            let cover = ui.radio_value(&mut self.fit, Fit::Cover, "cover").changed();
            let contain = ui
                .radio_value(&mut self.fit, Fit::Contain, "contain")
                .changed();
            if cover || contain {
                self.invalidate_preview();
            }
            ui.label("FFmpeg:");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.ffmpeg)
                        .desired_width(ui.available_width()),
                )
                .changed()
            {
                self.invalidate_preview();
            }
            if self.preview_job.is_none() {
                if ui.button("Preview selection").clicked() {
                    self.preview();
                }
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    if ui.button("Cancel preview").clicked() {
                        self.preview_job
                            .as_ref()
                            .unwrap()
                            .cancel
                            .store(true, Ordering::Relaxed);
                    }
                });
            }
            egui::CollapsingHeader::new("Profiles").show(ui, |ui| {
                egui::ComboBox::from_id_source("playback-profile")
                    .selected_text(if self.profile_name.is_empty() {
                        "Choose a profile"
                    } else {
                        &self.profile_name
                    })
                    .show_ui(ui, |ui| {
                        for name in self.library.profiles.keys() {
                            ui.selectable_value(&mut self.profile_name, name.clone(), name);
                        }
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut self.profile_name)
                        .hint_text("Profile name")
                        .desired_width(ui.available_width()),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            !self.profile_name.trim().is_empty(),
                            egui::Button::new("Save profile"),
                        )
                        .clicked()
                    {
                        self.library
                            .profiles
                            .insert(self.profile_name.trim().to_owned(), self.current_profile());
                    }
                    if ui.button("Load").clicked() {
                        if let Some(profile) =
                            self.library.profiles.get(self.profile_name.trim()).cloned()
                        {
                            self.apply_profile(&profile);
                            self.status =
                                "Profile selected. Press Play to apply it to the desktop.".into();
                        }
                    }
                    if ui.button("Delete").clicked() {
                        self.library.profiles.remove(self.profile_name.trim());
                    }
                });
            });
            egui::CollapsingHeader::new(format!("Playlist ({})", self.library.playlist.len()))
                .id_source("playlist-controls")
                .show(ui, |ui| {
                    if ui.button("Add selection").clicked() {
                        self.add_to_playlist();
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Interval (seconds):");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.library.interval_seconds)
                                    .range(1..=86400),
                            )
                            .changed()
                            && self.playlist_deadline.is_some()
                        {
                            self.playlist_deadline = Some(
                                Instant::now() + Duration::from_secs(self.library.interval_seconds),
                            );
                        }
                    });
                    if ui
                        .checkbox(&mut self.library.shuffle, "Shuffle without repeats")
                        .changed()
                    {
                        self.shuffle_bag.clear();
                    }
                    ui.horizontal_wrapped(|ui| {
                        let active = self.playlist_deadline.is_some();
                        if ui
                            .add_enabled(
                                !self.library.playlist.is_empty(),
                                egui::Button::new(if active { "Next" } else { "Start playlist" }),
                            )
                            .clicked()
                        {
                            self.advance_playlist();
                        }
                        if active && ui.button("Pause rotation").clicked() {
                            self.playlist_deadline = None;
                        }
                    });
                    if let Some(deadline) = self.playlist_deadline {
                        ui.small(format!(
                            "Next in {}s",
                            deadline.saturating_duration_since(Instant::now()).as_secs()
                        ));
                    }
                    let mut remove = None;
                    let mut movement = None;
                    let mut selected = None;
                    for (index, path) in self.library.playlist.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(index);
                                }
                                if ui
                                    .add_enabled(index > 0, egui::Button::new("Up").small())
                                    .clicked()
                                {
                                    movement = Some((index, index - 1));
                                }
                                if ui
                                    .add_enabled(
                                        index + 1 < self.library.playlist.len(),
                                        egui::Button::new("Down").small(),
                                    )
                                    .clicked()
                                {
                                    movement = Some((index, index + 1));
                                }
                                let title = self
                                    .projects
                                    .iter()
                                    .find(|entry| entry.path == *path)
                                    .map(|entry| entry.title.clone())
                                    .unwrap_or_else(|| path.display().to_string());
                                if ui
                                    .selectable_label(Path::new(&self.media_path) == path, title)
                                    .on_hover_text(path.display().to_string())
                                    .clicked()
                                {
                                    selected = Some(path.clone());
                                }
                            });
                        });
                    }
                    if let Some(path) = selected {
                        self.media_path = path.to_string_lossy().into_owned();
                        self.preview_flag = false;
                        self.selection_changed();
                    }
                    if let Some(index) = remove {
                        self.library.playlist.remove(index);
                        self.shuffle_bag.clear();
                        if self.library.playlist.is_empty() {
                            self.playlist_deadline = None;
                        }
                    } else if let Some((from, to)) = movement {
                        self.library.playlist.swap(from, to);
                    }
                });
            ui.separator();
            if ui.button("Save settings").clicked() {
                self.save_settings();
            }
            ui.small("Favorites, tags, folders, profiles and the last selection are saved on close. Playback never starts automatically.");
            if let Some(error) = &self.settings_error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        }
        fn selection_details(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
            let entry = self
                .projects
                .iter()
                .find(|entry| self.selection.as_ref() == Some(&entry.path))
                .cloned();
            let texture = self.preview_texture.clone().or_else(|| {
                entry
                    .as_ref()
                    .and_then(|entry| self.thumbnail(ctx, &entry.path))
            });
            let width = ui.available_width();
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(width, width * 0.625), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 3.0, egui::Color32::from_rgb(14, 21, 30));
            if let Some(texture) = texture {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let image_rect = egui::Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "WALLPAPER PREVIEW",
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_rgb(112, 133, 154),
                );
            }
            ui.add_space(8.0);
            let title = entry
                .as_ref()
                .map(|entry| entry.title.as_str())
                .unwrap_or_else(|| {
                    if self.media_path.is_empty() {
                        "Choose a wallpaper"
                    } else {
                        Path::new(&self.media_path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("Selected wallpaper")
                    }
                });
            ui.label(egui::RichText::new(title).size(19.0).strong());
            if let Some(entry) = entry {
                ui.horizontal_wrapped(|ui| {
                    ui.small(entry.kind.to_uppercase());
                    ui.colored_label(
                        if entry.supported {
                            egui::Color32::from_rgb(100, 196, 154)
                        } else {
                            egui::Color32::from_rgb(227, 177, 99)
                        },
                        if entry.supported {
                            "Supported"
                        } else {
                            "Not supported"
                        },
                    );
                });
                ui.small(&entry.support_note);
                if self.preview_texture.is_none() && entry.preview.is_some() {
                    ui.small("Project thumbnail, not a rendered wallpaper frame.");
                }
            } else {
                ui.small("Select a tile to see its preview and properties, or drop a local file into the window.");
            }
            ui.separator();
            self.playback_controls(ui);
            if let Some(path) = self.selection.clone() {
                egui::CollapsingHeader::new("Tags").show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.tag_input)
                            .desired_width(ui.available_width())
                            .hint_text("Comma-separated tags"),
                    );
                    if ui.button("Apply tags").clicked() {
                        let mut tags = Vec::new();
                        for tag in self
                            .tag_input
                            .split(',')
                            .map(str::trim)
                            .filter(|tag| !tag.is_empty())
                        {
                            if !tags
                                .iter()
                                .any(|existing: &String| existing.eq_ignore_ascii_case(tag))
                            {
                                tags.push(tag.to_owned());
                            }
                        }
                        if tags.is_empty() {
                            self.library.tags.remove(&path);
                        } else {
                            self.library.tags.insert(path, tags);
                        }
                    }
                });
            }
            if !self.inspect_json.is_empty() {
                egui::CollapsingHeader::new("Inspection details").show(ui, |ui| {
                    egui::ScrollArea::both()
                        .id_source("inspection-details")
                        .max_height(200.0)
                        .show(ui, |ui| {
                            ui.monospace(&self.inspect_json);
                        });
                });
            }
            if crate::gnome::is_current() {
                ui.separator();
                ui.small("GNOME: animation requires the optional GNOME 50 Shell bridge. Static images need no extension.");
            } else if crate::plasma::is_current() {
                ui.separator();
                ui.small("KDE Plasma: static images only, on all displays in the current activity. Leave output selection unset; use project preview for scenes.");
            }
        }

        fn selection_bar(&mut self, ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                ui.small("Selected:");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.media_path)
                            .desired_width(ui.available_width())
                            .hint_text("Select a wallpaper or enter a media path"),
                    )
                    .changed()
                {
                    self.selection_changed();
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.media_path.is_empty(),
                        egui::Button::new("Apply Wallpaper")
                            .fill(egui::Color32::from_rgb(36, 120, 181))
                            .min_size(egui::vec2(140.0, 30.0)),
                    )
                    .on_hover_text("Play selection (Ctrl+Enter)")
                    .clicked()
                {
                    self.playlist_deadline = None;
                    self.play();
                }
                if ui
                    .add_enabled(self.player.has_pending_work(), egui::Button::new("Stop"))
                    .on_hover_text("Stop wallpaper and playlist rotation (Esc outside text fields)")
                    .clicked()
                {
                    self.stop();
                }
                if ui.button("Inspect").clicked() {
                    self.inspect();
                }
                if ui
                    .checkbox(&mut self.preview_flag, "Use project preview")
                    .changed()
                {
                    self.invalidate_preview();
                }
                if ui
                    .add_enabled(self.selection.is_some(), egui::Button::new("Favorite"))
                    .clicked()
                {
                    if let Some(path) = self.selection.clone() {
                        self.toggle_favorite(&path);
                    }
                }
                if ui.button("Add to playlist").clicked() {
                    self.add_to_playlist();
                }
                if self.playlist_deadline.is_some() && ui.button("Next wallpaper").clicked() {
                    self.advance_playlist();
                }
            });
        }

        fn playback_status(&self, ui: &mut egui::Ui) {
            ui.small(&self.status);
            if let Some(status) = self.player.exit_status() {
                let message = format!("Player exited: {status}");
                ui.colored_label(
                    if status.success() {
                        ui.visuals().text_color()
                    } else {
                        egui::Color32::LIGHT_RED
                    },
                    message,
                );
            }
            let diagnostics = self.player.diagnostics();
            if !diagnostics.is_empty() {
                egui::ScrollArea::vertical()
                    .id_source("player-diagnostics")
                    .max_height(80.0)
                    .show(ui, |ui| {
                        ui.monospace(diagnostics);
                    });
            }
            if let Some(error) = &self.settings_error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        }

        /// Draw and poll the panel, also usable without a native window.
        pub fn show(&mut self, ctx: &egui::Context) {
            if !self.styled {
                let mut visuals = egui::Visuals::dark();
                visuals.panel_fill = egui::Color32::from_rgb(23, 31, 42);
                visuals.window_fill = egui::Color32::from_rgb(28, 38, 50);
                visuals.extreme_bg_color = egui::Color32::from_rgb(13, 21, 30);
                visuals.faint_bg_color = egui::Color32::from_rgb(32, 43, 56);
                visuals.selection.bg_fill = egui::Color32::from_rgb(36, 103, 151);
                visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
                visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 48, 62);
                visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 71, 93);
                visuals.widgets.active.bg_fill = egui::Color32::from_rgb(41, 114, 169);
                ctx.set_visuals(visuals);
                self.styled = true;
            }
            self.poll_jobs(ctx);
            let running = match self.player.is_running() {
                Ok(running) => running,
                Err(error) => {
                    self.status = format!("Player monitoring failed: {error:#}");
                    false
                }
            };
            if let Some(deadline) = self.playlist_deadline {
                if !running {
                    self.playlist_deadline = None;
                    self.status = "Player exited; playlist rotation paused. Check the diagnostics before restarting.".into();
                } else if Instant::now() >= deadline {
                    self.advance_playlist();
                }
            }
            let focus_search =
                ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::F));
            if focus_search {
                ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("library-search")));
            }
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)) {
                self.playlist_deadline = None;
                self.play();
            }
            if !ctx.wants_keyboard_input() {
                if ctx
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    self.stop();
                }
                let previous = ctx.input_mut(|input| {
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                });
                let next = ctx.input_mut(|input| {
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                });
                if previous || next {
                    let entries = self.visible_projects();
                    if !entries.is_empty() {
                        let index = entries
                            .iter()
                            .position(|entry| self.selection.as_ref() == Some(&entry.path));
                        let index = if previous {
                            index.map_or(entries.len() - 1, |index| index.saturating_sub(1))
                        } else {
                            index.map_or(0, |index| (index + 1).min(entries.len() - 1))
                        };
                        self.select_project(&entries[index]);
                    }
                }
            }
            for file in ctx.input(|input| input.raw.dropped_files.clone()) {
                if let Some(path) = file.path {
                    let Some(path_text) = path.to_str() else {
                        self.status =
                            "The GUI requires UTF-8 paths; use the CLI for this filename.".into();
                        continue;
                    };
                    if path.is_dir() {
                        self.folder_input = path_text.to_owned();
                        self.add_folder();
                    } else {
                        self.media_path = path_text.to_owned();
                        self.preview_flag = false;
                        self.selection_changed();
                        self.status = "Dropped file selected. Inspect or Play.".to_string();
                    }
                }
            }
            egui::TopBottomPanel::top("wallbreak-top")
                .min_height(76.0)
                .frame(egui::Frame::none().fill(egui::Color32::from_rgb(29, 41, 55)))
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("wallbreak").size(23.0).strong());
                        ui.small("Wallpaper Library");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Refresh library").clicked() {
                                self.refresh_projects();
                            }
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(10.0);
                        if ui
                            .selectable_label(
                                !self.playlist_view && !self.library.favorites_only,
                                "Installed",
                            )
                            .clicked()
                        {
                            self.playlist_view = false;
                            self.library.favorites_only = false;
                        }
                        if ui
                            .selectable_label(
                                !self.playlist_view && self.library.favorites_only,
                                "Favorites",
                            )
                            .clicked()
                        {
                            self.playlist_view = false;
                            self.library.favorites_only = true;
                        }
                        if ui
                            .selectable_label(self.playlist_view, "Playlist")
                            .clicked()
                        {
                            self.playlist_view = true;
                            self.library.favorites_only = false;
                        }
                        if let Some(job) = &self.scan {
                            ui.spinner();
                            ui.label("Loading library");
                            if ui.button("Cancel scan").clicked() {
                                job.cancel.store(true, Ordering::Relaxed);
                                self.scan_again = false;
                            }
                        }
                    });
                    ui.add_space(8.0);
                    if self.player.has_pending_work() {
                        ui.small(format!(
                            "{}: {}",
                            if running { "Running" } else { "Cleaning up" },
                            self.player.description()
                        ));
                    }
                });
            egui::TopBottomPanel::bottom("wallbreak-actions")
                .min_height(if ctx.screen_rect().width() < 850.0 {
                    116.0
                } else {
                    90.0
                })
                .frame(
                    egui::Frame::none()
                        .inner_margin(10.0)
                        .fill(egui::Color32::from_rgb(26, 36, 48)),
                )
                .show(ctx, |ui| {
                    self.selection_bar(ui);
                });
            let narrow = ctx.screen_rect().width() < 720.0;
            let inspector = ctx.screen_rect().width() >= 1050.0;
            if !narrow {
                egui::SidePanel::left("wallbreak-filters")
                    .resizable(true)
                    .default_width(185.0)
                    .width_range(160.0..=230.0)
                    .frame(
                        egui::Frame::none()
                            .inner_margin(12.0)
                            .fill(egui::Color32::from_rgb(23, 32, 44)),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("browse-scroll")
                            .show(ui, |ui| {
                                self.library_controls(ui);
                            });
                    });
            }
            if inspector {
                egui::SidePanel::right("wallbreak-properties")
                    .resizable(true)
                    .default_width(290.0)
                    .width_range(250.0..=380.0)
                    .frame(
                        egui::Frame::none()
                            .inner_margin(12.0)
                            .fill(egui::Color32::from_rgb(28, 38, 50)),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("wallpaper-properties-scroll")
                            .show(ui, |ui| {
                                self.selection_details(ui, ctx);
                            });
                    });
            }
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::none()
                        .inner_margin(14.0)
                        .fill(egui::Color32::from_rgb(16, 24, 34)),
                )
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(if self.playlist_view {
                                "Wallpaper Playlist"
                            } else if self.library.favorites_only {
                                "Favorite Wallpapers"
                            } else {
                                "Installed Wallpapers"
                            })
                            .size(20.0)
                            .strong(),
                        );
                        ui.small(format!("{} available", self.projects.len()));
                    });
                    ui.add_space(6.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(egui::Id::new("library-search"))
                            .desired_width(ui.available_width())
                            .hint_text("Search wallpapers, types and tags  (Ctrl+F)"),
                    );
                    self.playback_status(ui);
                    if !inspector {
                        egui::CollapsingHeader::new("Browse & Playback")
                            .default_open(false)
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_source("narrow-browse-scroll")
                                    .max_height(ctx.screen_rect().height() * 0.4)
                                    .show(ui, |ui| {
                                        if narrow {
                                            self.library_controls(ui);
                                        }
                                        self.selection_details(ui, ctx);
                                    });
                            });
                        ui.separator();
                    }
                    if !self.catalog_warnings.is_empty() {
                        egui::CollapsingHeader::new(format!(
                            "Library warnings ({})",
                            self.catalog_warnings.len()
                        ))
                        .id_source("library-warnings")
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_source("library-warning-details")
                                .max_height(100.0)
                                .show(ui, |ui| {
                                    for warning in &self.catalog_warnings {
                                        ui.label(warning);
                                    }
                                });
                        });
                    }
                    ui.separator();
                    let visible = self.visible_projects();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "{} shown ({} available)",
                            visible.len(),
                            self.projects.len()
                        ));
                        ui.selectable_value(&mut self.library.list_view, false, "Grid");
                        ui.selectable_value(&mut self.library.list_view, true, "List");
                        if self.playlist_view {
                            if ui
                                .button(if self.playlist_deadline.is_some() {
                                    "Next wallpaper"
                                } else {
                                    "Start playlist"
                                })
                                .clicked()
                            {
                                self.advance_playlist();
                            }
                            if self.playlist_deadline.is_some()
                                && ui.button("Pause rotation").clicked()
                            {
                                self.playlist_deadline = None;
                            }
                        }
                    });
                    ui.add_space(6.0);
                    if visible.is_empty() {
                        ui.label(
                        "No matching wallpapers. Add a media folder or refresh your Steam library.",
                    );
                    }
                    let spacing = 10.0;
                    egui::ScrollArea::vertical()
                        .id_source("project-gallery")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.library.list_view {
                                for entry in &visible {
                                    ui.push_id(&entry.path, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            if ui
                                                .selectable_label(
                                                    self.library.favorites.contains(&entry.path),
                                                    "Fav",
                                                )
                                                .clicked()
                                            {
                                                self.toggle_favorite(&entry.path);
                                            }
                                            ui.small(&entry.kind);
                                            let response = ui
                                                .selectable_label(
                                                    self.selection.as_ref() == Some(&entry.path),
                                                    &entry.title,
                                                )
                                                .on_hover_text(format!(
                                                    "{}\n{}",
                                                    entry.path.display(),
                                                    entry.support_note
                                                ));
                                            self.project_interaction(entry, &response);
                                            if !entry.supported {
                                                ui.small("unsupported");
                                            }
                                        });
                                    });
                                    ui.separator();
                                }
                            } else {
                                let avail = ui.available_width();
                                let target = self.library.card_size.min(avail);
                                let columns = (((avail + spacing) / (target + spacing)).floor()
                                    as usize)
                                    .clamp(1, 8);
                                let card_width = ((avail - (columns - 1) as f32 * spacing)
                                    / columns as f32)
                                    .max(1.0);
                                for row in visible.chunks(columns) {
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing =
                                            egui::vec2(spacing, spacing);
                                        for entry in row {
                                            ui.push_id(&entry.path, |ui| {
                                                self.project_card(ui, ctx, entry, card_width);
                                            });
                                        }
                                    });
                                }
                            }
                        });
                });
            if self.player.has_pending_work()
                || self.scan.is_some()
                || self.output_scan.is_some()
                || self.preview_job.is_some()
                || self.playlist_deadline.is_some()
            {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    impl Drop for Panel {
        fn drop(&mut self) {
            if let Some(job) = &self.scan {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(job) = &self.output_scan {
                job.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(job) = &self.preview_job {
                job.cancel.store(true, Ordering::Relaxed);
            }
            self.library.playback = self.current_profile();
            if self.settings_path.is_some() && self.saved_settings.as_ref() != Some(&self.library) {
                self.save_settings();
                if let Some(error) = &self.settings_error {
                    eprintln!("wallbreak: {error}");
                }
            }
        }
    }

    impl eframe::App for Panel {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.show(ctx);
        }
    }
}
