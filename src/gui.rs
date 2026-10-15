//! Graphical control panel for wallbreak.
//!
//! The panel itself never renders wallpapers. It manages one `wallbreak play`
//! child process per user action and renders single preview frames in-process
//! through the library, without touching Wayland or the desktop.

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result};

use crate::{
    display,
    media::{Fit, MediaWorker, PlaybackOptions},
    project::{self, MediaKind},
};

/// A managed `wallbreak play` child process.
///
/// Dropping the player terminates and reaps the child, so a closed panel can
/// never leak a wallpaper surface.
#[derive(Default)]
pub struct Player {
    child: Option<Child>,
    description: String,
}

impl Player {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => return true,
                _ => self.child = None,
            }
        }
        false
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Start any program with arguments, replacing a running player.
    pub fn start(&mut self, program: &PathBuf, args: &[String], description: String) -> Result<()> {
        self.stop()?;
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", program.display()))?;
        self.child = Some(child);
        self.description = description;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                child.kill().context("cannot terminate wallpaper process")?;
            }
            child.wait().context("cannot reap wallpaper process")?;
        }
        self.description.clear();
        Ok(())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.stop();
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
    fn player_starts_and_stops_an_owned_process() {
        let mut player = Player::new();
        assert!(!player.is_running());
        player
            .start(&PathBuf::from("sleep"), &["30".to_string()], "sleep".into())
            .unwrap();
        assert!(player.is_running());
        assert_eq!(player.description(), "sleep");
        player.stop().unwrap();
        assert!(!player.is_running());
        assert!(player.description().is_empty());
    }

    #[test]
    fn player_replaces_a_running_process() {
        let mut player = Player::new();
        player
            .start(&PathBuf::from("sleep"), &["30".to_string()], "first".into())
            .unwrap();
        let first = player.child.as_ref().unwrap().id();
        player
            .start(
                &PathBuf::from("sleep"),
                &["30".to_string()],
                "second".into(),
            )
            .unwrap();
        assert_ne!(player.child.as_ref().unwrap().id(), first);
        assert_eq!(player.description(), "second");
        player.stop().unwrap();
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
        }

        pub fn refresh(&mut self) {
            self.refresh_projects();
            match refresh_outputs() {
                Ok(outputs) => {
                    if !outputs.contains(&self.output) {
                        self.output.clear();
                    }
                    self.outputs = outputs;
                    self.status = "Outputs refreshed.".to_string();
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
                Ok(()) => self.status = "Playing. Stop removes the wallpaper.".to_string(),
                Err(error) => self.status = format!("Play failed: {error:#}"),
            }
        }

        fn stop(&mut self) {
            match self.player.stop() {
                Ok(()) => self.status = "Stopped.".to_string(),
                Err(error) => self.status = format!("Stop failed: {error:#}"),
            }
        }
    }

    impl eframe::App for Panel {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            for file in ctx.input(|input| input.raw.dropped_files.clone()) {
                if let Some(path) = file.path {
                    self.media_path = path.to_string_lossy().into_owned();
                    self.status = "Dropped file selected. Inspect or Play.".to_string();
                }
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("wallbreak");
                ui.label("Drop a media file here, or type its path.");
                ui.horizontal(|ui| {
                    ui.label("Steam:");
                    let selected = self
                        .projects
                        .iter()
                        .find(|entry| entry.id == self.project)
                        .map(|entry| {
                            format!(
                                "{} [{}]{}",
                                entry.title,
                                entry.kind,
                                if entry.supported {
                                    ""
                                } else {
                                    " (unsupported)"
                                }
                            )
                        })
                        .unwrap_or_else(|| "none".to_string());
                    egui::ComboBox::from_id_source("project")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for entry in self.projects.clone() {
                                let label = format!(
                                    "{} [{}]{}",
                                    entry.title,
                                    entry.kind,
                                    if entry.supported {
                                        ""
                                    } else {
                                        " (unsupported)"
                                    }
                                );
                                if ui
                                    .selectable_value(&mut self.project, entry.id.clone(), label)
                                    .clicked()
                                {
                                    self.media_path = entry.path.to_string_lossy().into_owned();
                                    self.preview_flag = false;
                                    self.status = entry.support_note.clone();
                                }
                            }
                        });
                    if ui.button("Rescan").clicked() {
                        self.refresh_projects();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Media:");
                    ui.text_edit_singleline(&mut self.media_path);
                    if ui.button("Inspect").clicked() {
                        self.inspect();
                    }
                });
                ui.checkbox(
                    &mut self.preview_flag,
                    "Use project preview instead of rendering",
                );
                ui.horizontal(|ui| {
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
                });
                ui.horizontal(|ui| {
                    ui.label("FPS:");
                    ui.add(egui::Slider::new(&mut self.fps, 1..=60));
                    ui.label("Fit:");
                    ui.radio_value(&mut self.fit, Fit::Cover, "cover");
                    ui.radio_value(&mut self.fit, Fit::Contain, "contain");
                });
                ui.horizontal(|ui| {
                    ui.label("FFmpeg:");
                    ui.text_edit_singleline(&mut self.ffmpeg);
                });
                ui.horizontal(|ui| {
                    if ui.button("Preview").clicked() {
                        self.preview(ctx);
                    }
                    if self.player.is_running() {
                        if ui.button("Stop").clicked() {
                            self.stop();
                        }
                        ui.label(format!("Running: {}", self.player.description()));
                    } else if ui.button("Play").clicked() {
                        self.play();
                    }
                });
                if let Some(texture) = &self.preview_texture {
                    ui.image(texture);
                }
                ui.separator();
                ui.label(&self.status);
                if !self.inspect_json.is_empty() {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.monospace(&self.inspect_json);
                        });
                }
            });
        }
    }
}
