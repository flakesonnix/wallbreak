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
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rustix::process::{kill_process, Pid, Signal};

use crate::{
    display,
    media::{Fit, MediaWorker, PlaybackOptions},
    project::{self, MediaKind},
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
        self.stop()?;
        self.exit_status = None;
        self.forced = false;
        self.diagnostics = Arc::new(Mutex::new(Diagnostics::default()));
        self.cancel_reader = Arc::new(AtomicBool::new(false));
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
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
                            return Err(error).context("cannot send KILL to wallpaper process")
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

/// Render one preview frame in-process. Images yield their fitted frame;
/// scenes and videos yield one decoded frame. Nothing is displayed on the desktop.
pub fn render_preview(
    path: &std::path::Path,
    preview: bool,
    fit: Fit,
    ffmpeg: &std::path::Path,
) -> Result<(Vec<u8>, u32, u32)> {
    let media = project::resolve(path, preview)?;
    if media.kind == MediaKind::Image {
        // Keep GUI previews cheap and bounded.
        let (width, height) = (320, 180);
        let options = PlaybackOptions {
            fps: 30,
            fit,
            ffmpeg: ffmpeg.to_path_buf(),
            duration: None,
        };
        let mut worker = MediaWorker::start(&media, width, height, &options)?;
        let frame = worker
            .take_frame()?
            .context("image produced no preview frame")?;
        worker.shutdown()?;
        Ok((frame, width, height))
    } else {
        let (width, height) = (320, 180);
        let options = PlaybackOptions {
            fps: 30,
            fit,
            ffmpeg: ffmpeg.to_path_buf(),
            duration: None,
        };
        let mut worker = MediaWorker::start(&media, width, height, &options)?;
        let start = std::time::Instant::now();
        let frame = loop {
            if let Some(frame) = worker.take_frame()? {
                break frame;
            }
            if start.elapsed() > std::time::Duration::from_secs(15) {
                anyhow::bail!("decoder produced no preview frame within 15 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        worker.shutdown()?;
        Ok((frame, width, height))
    }
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
        assert!(player
            .is_running()
            .unwrap_err()
            .to_string()
            .contains("poll"));
        assert!(player.has_pending_work());
        assert!(player.stop().is_err());
        assert_eq!(Pid::from_child(player.child.as_ref().unwrap()), pid);
        assert!(player
            .start(&PathBuf::from("must-not-run"), &[], "new".into())
            .is_err());
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
    use crate::steam::ProjectEntry;
    use eframe::egui;

    pub struct Panel {
        pub media_path: String,
        pub preview_flag: bool,
        pub projects: Vec<ProjectEntry>,
        pub project: String,
        pub search: String,
        pub kind_filter: String,
        pub supported_only: bool,
        pub thumbnails: std::collections::HashMap<String, egui::ColorImage>,
        pub thumb_textures: std::collections::HashMap<String, egui::TextureHandle>,
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
            self.projects = crate::steam::scan(&crate::steam::default_roots());
            if !self.projects.iter().any(|entry| entry.id == self.project) {
                self.project.clear();
            }
            // Bounded thumbnails for the gallery; failures keep text-only cards.
            self.thumbnails.clear();
            self.thumb_textures.clear();
            for entry in &self.projects {
                let Some(preview) = &entry.preview else {
                    continue;
                };
                let Ok(image) = image::open(preview) else {
                    continue;
                };
                let thumb = image.thumbnail(192, 192).into_rgba8();
                let (width, height) = (thumb.width() as usize, thumb.height() as usize);
                self.thumbnails.insert(
                    entry.id.clone(),
                    egui::ColorImage::from_rgba_unmultiplied([width, height], &thumb.into_raw()),
                );
            }
        }

        fn select_project(&mut self, entry: &ProjectEntry) {
            self.project = entry.id.clone();
            self.media_path = entry.path.to_string_lossy().into_owned();
            self.preview_flag = false;
            self.status = entry.support_note.clone();
        }

        fn visible_projects(&self) -> Vec<ProjectEntry> {
            let query = self.search.to_lowercase();
            self.projects
                .iter()
                .filter(|entry| {
                    (self.kind_filter == "All" || entry.kind == self.kind_filter)
                        && (!self.supported_only || entry.supported)
                        && (query.is_empty() || entry.title.to_lowercase().contains(&query))
                })
                .cloned()
                .collect()
        }

        /// One gallery card; returns true when selected by click.
        fn project_card(
            &mut self,
            ui: &mut egui::Ui,
            ctx: &egui::Context,
            entry: &ProjectEntry,
            card_width: f32,
        ) -> bool {
            let selected = self.project == entry.id;
            let mut clicked = false;
            egui::Frame::group(ui.style())
                .fill(if selected {
                    ui.style().visuals.selection.bg_fill
                } else {
                    ui.style().visuals.faint_bg_color
                })
                .show(ui, |ui| {
                    ui.set_width(card_width);
                    if let Some(image) = self.thumbnails.get(&entry.id).cloned() {
                        let texture =
                            self.thumb_textures
                                .entry(entry.id.clone())
                                .or_insert_with(|| {
                                    ctx.load_texture(
                                        format!("wallbreak-project-{}", entry.id),
                                        image,
                                        Default::default(),
                                    )
                                });
                        let aspect = texture.size_vec2();
                        // Fixed 16:10 viewport with center crop, like WE cards.
                        let target = egui::vec2(card_width, card_width * 10.0 / 16.0);
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
                        if ui
                            .add(
                                egui::ImageButton::new((texture.id(), target))
                                    .uv(uv)
                                    .selected(selected)
                                    .frame(false),
                            )
                            .clicked()
                        {
                            clicked = true;
                        }
                    }
                    let badge = if entry.supported { "✓" } else { "✗" };
                    ui.label(
                        egui::RichText::new(format!("{} {}", badge, entry.title))
                            .small()
                            .strong(),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button(&entry.kind).clicked() {
                            clicked = true;
                        }
                        if !entry.supported {
                            ui.small(egui::RichText::new("unsupported").weak());
                        }
                    });
                });
            clicked
        }

        pub fn refresh(&mut self) {
            self.refresh_projects();
            match refresh_outputs() {
                Ok(outputs) => {
                    if !outputs.contains(&self.output) {
                        self.output.clear();
                    }
                    self.outputs = outputs;
                    self.status = if crate::gnome::is_current() {
                        "GNOME: images use all displays; animation requires the optional Shell bridge and defaults to the primary display.".into()
                    } else {
                        "Outputs refreshed.".into()
                    };
                }
                Err(error) => self.status = format!("Outputs failed: {error:#}"),
            }
        }

        fn inspect(&mut self) {
            let path = PathBuf::from(self.media_path.trim());
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

        fn preview(&mut self, ctx: &egui::Context) {
            let path = PathBuf::from(self.media_path.trim());
            match render_preview(
                &path,
                self.preview_flag,
                self.fit,
                PathBuf::from(&self.ffmpeg).as_path(),
            ) {
                Ok((frame, width, height)) => {
                    let rgba = bgra_to_rgba(&frame);
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [width as usize, height as usize],
                        &rgba,
                    );
                    self.preview_texture = Some(ctx.load_texture(
                        "wallbreak-preview",
                        image.clone(),
                        Default::default(),
                    ));
                    self.preview_image = Some(image);
                    self.status = format!("Preview: {width}x{height}.");
                }
                Err(error) => self.status = format!("Preview failed: {error:#}"),
            }
        }

        fn play(&mut self) {
            let binary = match wallbreak_binary() {
                Ok(binary) => binary,
                Err(error) => {
                    self.status = format!("Play failed: {error:#}");
                    return;
                }
            };
            let mut args = vec![
                "play".to_string(),
                self.media_path.trim().to_string(),
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
                self.media_path.trim(),
                self.selected_output()
                    .unwrap_or_else(|| "default output".to_string())
            );
            match self.player.start(&binary, &args, description) {
                Ok(()) => {
                    self.status = "Starting player. Stop requests wallpaper cleanup.".to_string()
                }
                Err(error) => self.status = format!("Play failed: {error:#}"),
            }
        }

        fn stop(&mut self) {
            match self.player.stop() {
                Ok(()) => self.status = "Stopped.".to_string(),
                Err(error) => self.status = format!("Stop failed: {error:#}"),
            }
        }
        /// Draw and poll the panel, also usable without a native window.
        pub fn show(&mut self, ctx: &egui::Context) {
            let running = match self.player.is_running() {
                Ok(running) => running,
                Err(error) => {
                    self.status = format!("Player monitoring failed: {error:#}");
                    false
                }
            };
            for file in ctx.input(|input| input.raw.dropped_files.clone()) {
                if let Some(path) = file.path {
                    self.media_path = path.to_string_lossy().into_owned();
                    self.status = "Dropped file selected. Inspect or Play.".to_string();
                }
            }
            egui::TopBottomPanel::top("wallbreak-top").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("wallbreak");
                    ui.label("Drop a media file anywhere, or pick a wallpaper below.");
                    if ui.button("Rescan Steam").clicked() {
                        self.refresh_projects();
                    }
                    if self.player.has_pending_work() {
                        if ui.button("⏹ Stop").clicked() {
                            self.stop();
                        }
                        ui.label(format!(
                            "{}: {}",
                            if running {
                                "Running"
                            } else {
                                "Cleaning up"
                            },
                            self.player.description()
                        ));
                    } else if ui.button("▶ Play").clicked() {
                        self.play();
                    }
                });
            });
            if crate::gnome::is_current() {
                egui::TopBottomPanel::top("wallbreak-gnome-note").show(ctx, |ui| {
                    ui.label("GNOME: video/scene animation requires the optional GNOME 50 Shell bridge. Static images need no extension.");
                });
            }
            egui::SidePanel::left("wallbreak-filters")
                .resizable(true)
                .default_width(210.0)
                .show(ctx, |ui| {
                    ui.heading("Browse");
                    ui.label("Search:");
                    ui.text_edit_singleline(&mut self.search);
                    ui.label("Type:");
                    egui::ComboBox::from_id_source("kind-filter")
                        .selected_text(&self.kind_filter)
                        .show_ui(ui, |ui| {
                            for kind in ["All", "scene", "video", "web", "unknown"] {
                                ui.selectable_value(&mut self.kind_filter, kind.to_string(), kind);
                            }
                        });
                    ui.checkbox(&mut self.supported_only, "Supported only");
                    ui.separator();
                    ui.heading("Playback");
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
                        self.refresh();
                    }
                    ui.label("FPS:");
                    ui.add(egui::Slider::new(&mut self.fps, 1..=60));
                    ui.label("Fit:");
                    ui.radio_value(&mut self.fit, Fit::Cover, "cover");
                    ui.radio_value(&mut self.fit, Fit::Contain, "contain");
                    ui.label("FFmpeg:");
                    ui.text_edit_singleline(&mut self.ffmpeg);
                    if ui.button("Preview selection").clicked() {
                        self.preview(ctx);
                    }
                    if let Some(texture) = &self.preview_texture {
                        let size = texture.size_vec2();
                        let width = ui.available_width().min(320.0);
                        ui.image((
                            texture.id(),
                            egui::vec2(width, width * size.y / size.x.max(1.0)),
                        ));
                    }
                });
            egui::CentralPanel::default().show(ctx, |ui| {
                let visible = self.visible_projects();
                ui.horizontal(|ui| {
                    ui.heading("Wallpapers");
                    ui.label(format!(
                        "{} shown ({} installed)",
                        visible.len(),
                        self.projects.len()
                    ));
                });
                if visible.is_empty() {
                    ui.label("No matching Wallpaper Engine projects.");
                }
                // Responsive grid: column count follows the window width.
                let spacing = 10.0;
                let avail = ui.available_width();
                let columns = ((avail / 200.0).floor() as usize).clamp(1, 8);
                let card_width =
                    ((avail - (columns - 1) as f32 * spacing) / columns as f32).max(120.0);
                egui::ScrollArea::vertical()
                    .id_source("project-gallery")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in visible.chunks(columns) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                                for entry in row {
                                    if self.project_card(ui, ctx, entry, card_width) {
                                        self.select_project(entry);
                                    }
                                }
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Selected:");
                    ui.text_edit_singleline(&mut self.media_path);
                    if ui.button("Inspect").clicked() {
                        self.inspect();
                    }
                    ui.checkbox(&mut self.preview_flag, "preview image");
                });
                ui.separator();
                ui.label(&self.status);
                if let Some(status) = self.player.exit_status() {
                    let message = format!("Player exited: {status}");
                    if status.success() {
                        ui.label(message);
                    } else {
                        ui.colored_label(egui::Color32::LIGHT_RED, message);
                    }
                }
                let diagnostics = self.player.diagnostics();
                if !diagnostics.is_empty() {
                    egui::ScrollArea::vertical()
                        .id_source("player-diagnostics")
                        .max_height(160.0)
                        .show(ui, |ui| {
                            ui.monospace(diagnostics);
                        });
                }
                if !self.inspect_json.is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.monospace(&self.inspect_json);
                        });
                }
            });
            if self.player.has_pending_work() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    impl eframe::App for Panel {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.show(ctx);
        }
    }
}
