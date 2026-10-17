//! Ordinary, bridge-owned Wayland source window for GNOME animation clones.

use std::{
    cell::RefCell,
    os::{fd::BorrowedFd, unix::fs::FileTypeExt},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use eframe::egui;

use crate::{
    gui::bgra_to_rgba,
    media::{frame_len, Fit, MediaWorker, PlaybackOptions},
    project,
};

pub const APP_ID: &str = "io.github.flakesonnix.wallbreak.Renderer";
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL: Duration = Duration::from_millis(100);
const FIRST_FRAME_POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Parser)]
#[command(
    name = "wallbreak-renderer",
    about = "GNOME bridge-owned animation source (not a standalone player)"
)]
pub struct Args {
    #[arg(long)]
    source: PathBuf,
    #[arg(long, value_parser = session_token)]
    session_token: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    width: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    height: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=16_384))]
    logical_width: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=16_384))]
    logical_height: u32,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,
    #[arg(long, value_enum)]
    fit: Fit,
    #[arg(long)]
    ffmpeg: PathBuf,
}

fn session_token(value: &str) -> std::result::Result<String, String> {
    let valid = value.len() == 36
        && value.bytes().enumerate().all(|(i, byte)| {
            if [8, 13, 18, 23].contains(&i) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        });
    if valid {
        Ok(value.to_owned())
    } else {
        Err("session token must be a lowercase hexadecimal UUID (8-4-4-4-12)".into())
    }
}

impl Args {
    fn validate(&self) -> Result<()> {
        // Admit both the media allocation and the minimum native surface before I/O.
        frame_len(self.width, self.height)?;
        frame_len(self.logical_width, self.logical_height)
            .context("logical window dimensions exceed the surface budget")?;
        ensure!(
            (1..=16_384).contains(&self.logical_width)
                && (1..=16_384).contains(&self.logical_height),
            "logical dimensions must be between 1 and 16384"
        );
        ensure!(
            self.source.is_absolute(),
            "--source must be an absolute file path"
        );
        ensure!(
            !self.ffmpeg.as_os_str().is_empty(),
            "--ffmpeg must not be empty"
        );
        Ok(())
    }

    fn native_options(&self) -> eframe::NativeOptions {
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title(format!("wallbreak-renderer:{}", self.session_token))
                .with_app_id(APP_ID)
                .with_inner_size([self.logical_width as f32, self.logical_height as f32])
                .with_clamp_size_to_monitor_size(false)
                .with_decorations(false)
                .with_fullscreen(false)
                .with_resizable(false)
                .with_mouse_passthrough(true)
                .with_icon(egui::IconData::default()),
            run_and_return: true,
            persist_window: false,
            // A hidden clone source cannot rely on compositor frame delivery.
            // Animation::show already paces frames; EGL must not gate shutdown.
            vsync: false,
            ..Default::default()
        }
    }
}

fn require_owned_socket() -> Result<()> {
    let value = std::env::var("WAYLAND_SOCKET")
        .context("WAYLAND_SOCKET is required; launch this renderer through the GNOME bridge")?;
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "WAYLAND_SOCKET must name an inherited socket descriptor"
    );
    let fd: i32 = value.parse().context("invalid WAYLAND_SOCKET descriptor")?;
    ensure!(fd > 2, "WAYLAND_SOCKET must not refer to standard I/O");
    ensure!(
        std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .context("WAYLAND_SOCKET descriptor is not open")?
            .file_type()
            .is_socket(),
        "WAYLAND_SOCKET descriptor is not a socket"
    );
    // The inherited, verified descriptor stays open until winit consumes it.
    // Do not let FFmpeg inherit it while we prepare the first frame before winit.
    let socket = unsafe { BorrowedFd::borrow_raw(fd) };
    let flags = rustix::io::fcntl_getfd(socket)?;
    rustix::io::fcntl_setfd(socket, flags | rustix::io::FdFlags::CLOEXEC)
        .context("cannot protect the bridge socket from decoder inheritance")?;
    Ok(())
}

fn frame_image(frame: &[u8], width: u32, height: u32) -> Result<egui::ColorImage> {
    ensure!(
        frame.len() == frame_len(width, height)?,
        "decoder returned an incomplete or incorrectly sized BGRA frame"
    );
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &bgra_to_rgba(frame),
    ))
}

fn with_cleanup(result: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (result, cleanup) {
        (Ok(()), cleanup) => cleanup,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("renderer cleanup also failed: {cleanup:#}")))
        }
    }
}

/// Run only with the private Wayland descriptor inherited from the native bridge.
/// Signal handlers set `close`; the GUI closes normally and all workers are joined.
/// An in-progress MediaWorker CPU render is not preemptible and must finish to join.
pub fn run(args: Args, close: Arc<AtomicBool>) -> Result<()> {
    args.validate()?;
    require_owned_socket()?;
    let options = args.native_options();
    let outcome = Rc::new(RefCell::new(Ok(())));
    let mut app = Animation::start(args, close, outcome.clone())?;
    let result = (|| {
        let Some(first) = app.first_frame(FIRST_FRAME_TIMEOUT)? else {
            return Ok(());
        };
        app.first = Some(first);
        if app.close.load(Ordering::Relaxed) {
            return Ok(());
        }
        eframe::run_native(APP_ID, options, Box::new(move |_cc| Ok(Box::new(app))))
            .map_err(|error| anyhow::anyhow!("renderer window/Wayland connection failed: {error}"))
    })();
    // The app (or the unused app-creator on window setup failure) has been dropped.
    with_cleanup(result, outcome.replace(Ok(())))
}

struct Animation {
    close: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
    requests: SyncSender<()>,
    frames: Receiver<Option<egui::ColorImage>>,
    in_flight: bool,
    first: Option<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    interval: Duration,
    next_frame: Instant,
    outcome: Rc<RefCell<Result<()>>>,
}

impl Animation {
    fn start(args: Args, close: Arc<AtomicBool>, outcome: Rc<RefCell<Result<()>>>) -> Result<Self> {
        args.validate()?;
        let interval = Duration::from_secs_f64(1.0 / f64::from(args.fps));
        let (requests, receive_request) = mpsc::sync_channel(1);
        let (send_frame, frames) = mpsc::sync_channel(1);
        let cancelled = close.clone();
        let worker = thread::Builder::new()
            .name("animation-frames".into())
            .spawn(move || {
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                ensure!(
                    args.source.is_file(),
                    "--source must be a regular media file, project.json or scene.json"
                );
                // Re-resolve the selected source, not a substituted project or preview.
                let media = project::resolve(&args.source, false)?;
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let options = PlaybackOptions {
                    fps: args.fps,
                    fit: args.fit,
                    ffmpeg: args.ffmpeg,
                    duration: None,
                };
                let mut worker = MediaWorker::start(&media, args.width, args.height, &options)?;
                let result = (|| {
                    while !cancelled.load(Ordering::Relaxed) {
                        match receive_request.recv_timeout(CANCEL_POLL) {
                            Ok(()) => {}
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let frame = worker.take_frame()?;
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let image = frame
                            .map(|frame| frame_image(&frame, args.width, args.height))
                            .transpose()?;
                        // One outstanding request bounds retained frames and never stalls the UI.
                        if send_frame.send(image).is_err() {
                            break;
                        }
                    }
                    Ok(())
                })();
                with_cleanup(result, worker.shutdown())
            })
            .context("cannot start animation frame worker")?;
        Ok(Self {
            close,
            worker: Some(worker),
            requests,
            frames,
            in_flight: false,
            first: None,
            texture: None,
            interval,
            next_frame: Instant::now(),
            outcome,
        })
    }

    fn request_frame(&mut self) -> Result<()> {
        if !self.in_flight {
            self.requests
                .try_send(())
                .context("animation frame worker stopped")?;
            self.in_flight = true;
        }
        Ok(())
    }

    fn first_frame(&mut self, timeout: Duration) -> Result<Option<egui::ColorImage>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.close.load(Ordering::Relaxed) {
                return Ok(None);
            }
            ensure!(
                Instant::now() < deadline,
                "decoder produced no complete first frame within {timeout:?}"
            );
            if let Err(error) = self.request_frame() {
                return if self.close.load(Ordering::Relaxed) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            match self.frames.recv_timeout(
                FIRST_FRAME_POLL.min(deadline.saturating_duration_since(Instant::now())),
            ) {
                Ok(image) => {
                    self.in_flight = false;
                    if self.close.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "decoder exceeded the first-frame timeout of {timeout:?}"
                    );
                    if image.is_some() {
                        return Ok(image);
                    }
                    thread::sleep(FIRST_FRAME_POLL);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if self.close.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    bail!("animation worker failed before its first complete frame")
                }
            }
        }
    }

    fn show(&mut self, ctx: &egui::Context) -> Result<()> {
        if self.close.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return Ok(());
        }
        let now = Instant::now();
        if now >= self.next_frame || self.first.is_some() {
            let image = if let Some(first) = self.first.take() {
                Some(first)
            } else if self.in_flight {
                match self.frames.try_recv() {
                    Ok(image) => {
                        self.in_flight = false;
                        image
                    }
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => bail!("animation frame worker disconnected"),
                }
            } else {
                None
            };
            if let Some(image) = image {
                let limit = ctx.input(|input| input.max_texture_side);
                ensure!(
                    image.width() <= limit && image.height() <= limit,
                    "frame exceeds the renderer's {limit}-pixel texture limit"
                );
                if let Some(texture) = &mut self.texture {
                    texture.set(image, egui::TextureOptions::LINEAR);
                } else {
                    self.texture =
                        Some(ctx.load_texture("animation", image, egui::TextureOptions::LINEAR));
                }
            }
            self.request_frame()?;
            self.next_frame = Instant::now() + self.interval;
        }
        if let Some(texture) = &self.texture {
            ctx.layer_painter(egui::LayerId::background()).image(
                texture.id(),
                ctx.screen_rect(),
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        let delay = self
            .next_frame
            .saturating_duration_since(Instant::now())
            .min(CANCEL_POLL);
        // egui 0.28 subtracts predicted_dt; compensate so 60 FPS does not become a busy loop.
        let prediction =
            ctx.input(|input| Duration::try_from_secs_f32(input.predicted_dt).unwrap_or_default());
        ctx.request_repaint_after(delay.saturating_add(prediction));
        Ok(())
    }
}

impl eframe::App for Animation {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Err(error) = self.show(ctx) {
            if !self.close.load(Ordering::Relaxed) {
                *self.outcome.borrow_mut() = Err(error);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        self.close.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let cleanup = worker
                .join()
                .map_err(|_| anyhow::anyhow!("animation frame worker panicked"))
                .and_then(|result| result);
            let previous = self.outcome.replace(Ok(()));
            *self.outcome.borrow_mut() = with_cleanup(previous, cleanup);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args::try_parse_from([
            "wallbreak-renderer",
            "--source",
            "/not-read-before-admission/project.json",
            "--session-token",
            "01234567-89ab-cdef-0123-456789abcdef",
            "--width",
            "8",
            "--height",
            "4",
            "--logical-width",
            "4",
            "--logical-height",
            "2",
            "--fps",
            "30",
            "--fit",
            "cover",
            "--ffmpeg",
            "ffmpeg",
        ])
        .unwrap()
    }

    #[test]
    fn token_is_exact_lowercase_uuid_without_normalization() {
        let token = "01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(session_token(token).unwrap(), token);
        for invalid in [
            token.to_uppercase(),
            token.replace('-', ""),
            format!(" {token}"),
            format!("{token}\n"),
            token.replace('a', "g"),
            token.replace('-', "_"),
            "\u{e9}".repeat(18),
        ] {
            assert!(session_token(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn admission_precedes_source_access_and_keeps_physical_and_logical_sizes_separate() {
        let mut args = args();
        args.validate().unwrap();
        let options = args.native_options();
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(4.0, 2.0)));
        assert_eq!(options.viewport.app_id.as_deref(), Some(APP_ID));
        assert_eq!(
            options.viewport.title.as_deref(),
            Some("wallbreak-renderer:01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(options.viewport.decorations, Some(false));
        assert_eq!(options.viewport.fullscreen, Some(false));
        assert_eq!(options.viewport.mouse_passthrough, Some(true));
        assert!(options.viewport.position.is_none());
        assert!(options.viewport.visible.is_none());
        assert!(options.viewport.active.is_none());
        assert!(!options.persist_window);
        assert!(options.run_and_return);
        assert!(
            !options.vsync,
            "hidden clone sources must not wait on presentation"
        );
        args.width = 4096;
        args.height = 4096;
        args.validate().unwrap();
        args.height += 1;
        assert!(args.validate().unwrap_err().to_string().contains("64 MiB"));
        args.width = u32::MAX;
        args.height = u32::MAX;
        assert!(args.validate().is_err());
        args.width = 0;
        assert!(args.validate().is_err());
        args.width = 8;
        args.height = 4;
        args.logical_width = 0;
        assert!(args.validate().is_err());
        args.logical_width = 16_385;
        assert!(args.validate().is_err());
        args.logical_width = 4096;
        args.logical_height = 4097;
        assert!(args.validate().is_err());
        args.logical_width = 4;
        args.logical_height = 2;
        args.source = "project.json".into();
        assert!(args
            .validate()
            .unwrap_err()
            .to_string()
            .contains("absolute"));
    }

    #[test]
    fn actual_frame_conversion_preserves_pixels_and_rejects_partial_frames() {
        let image = frame_image(&[3, 17, 241, 255, 89, 53, 1, 255], 2, 1).unwrap();
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(241, 17, 3));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(1, 53, 89));
        for length in [0, 3, 4, 7, 9] {
            assert!(frame_image(&vec![0; length], 2, 1).is_err());
        }
        assert!(frame_image(&[], u32::MAX, u32::MAX).is_err());
    }

    fn image_app() -> (tempfile::TempDir, Animation, Rc<RefCell<Result<()>>>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.png");
        image::RgbaImage::from_pixel(8, 4, image::Rgba([241, 17, 3, 255]))
            .save(&path)
            .unwrap();
        let mut args = args();
        args.source = path;
        let outcome = Rc::new(RefCell::new(Ok(())));
        let app =
            Animation::start(args, Arc::new(AtomicBool::new(false)), outcome.clone()).unwrap();
        (directory, app, outcome)
    }

    #[test]
    fn real_image_worker_paints_edge_to_edge_and_keeps_texture_without_new_frames() {
        let (_directory, mut app, outcome) = image_app();
        app.first = app.first_frame(Duration::from_secs(5)).unwrap();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| app.show(ctx).unwrap());
        let texture = app.texture.as_ref().unwrap().id();
        assert!(output.textures_delta.set.iter().any(|(id, delta)| {
            *id == texture
                && match &delta.image {
                    egui::ImageData::Color(image) => image
                        .pixels
                        .iter()
                        .all(|pixel| *pixel == egui::Color32::from_rgb(241, 17, 3)),
                    _ => false,
                }
        }));
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let mesh = primitives
            .iter()
            .find_map(|primitive| match &primitive.primitive {
                egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture => Some(mesh),
                _ => None,
            })
            .unwrap();
        assert!(mesh.vertices.iter().any(|v| v.pos == egui::pos2(0.0, 0.0)));
        assert!(mesh
            .vertices
            .iter()
            .any(|v| v.pos == egui::pos2(400.0, 200.0)));
        // The real image worker has no second frame. Polling must retain the first texture.
        thread::sleep(app.interval);
        for _ in 0..5 {
            let _ = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
        }
        let output = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
        assert_eq!(app.texture.as_ref().unwrap().id(), texture);
        assert!(!output
            .textures_delta
            .set
            .iter()
            .any(|(id, _)| *id == texture));
        assert!(output.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::ZERO);
        app.close.store(true, Ordering::Relaxed);
        let output = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close));
        drop(app);
        outcome.replace(Ok(())).unwrap();
    }

    #[test]
    fn cancellation_before_first_frame_joins_without_opening_media() {
        let outcome = Rc::new(RefCell::new(Ok(())));
        let mut app =
            Animation::start(args(), Arc::new(AtomicBool::new(true)), outcome.clone()).unwrap();
        assert!(app.first_frame(Duration::from_secs(1)).unwrap().is_none());
        drop(app);
        outcome.replace(Ok(())).unwrap();
    }

    #[test]
    fn fps_deadlines_remain_delayed_even_at_sixty_fps() {
        let (_directory, mut app, outcome) = image_app();
        app.first = app.first_frame(Duration::from_secs(5)).unwrap();
        let ctx = egui::Context::default();
        for fps in [1, 30, 60] {
            app.interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            app.next_frame = Instant::now() + app.interval;
            for _ in 0..5 {
                let _ = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
            }
            let output = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
            let delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            assert!(
                delay > Duration::ZERO,
                "{fps} FPS scheduled an immediate repaint"
            );
            assert!(delay <= app.interval.min(CANCEL_POLL));
        }
        drop(app);
        outcome.replace(Ok(())).unwrap();
    }

    #[test]
    fn startup_failure_survives_worker_join() {
        let outcome = Rc::new(RefCell::new(Ok(())));
        let mut app =
            Animation::start(args(), Arc::new(AtomicBool::new(false)), outcome.clone()).unwrap();
        assert!(app.first_frame(Duration::from_secs(5)).is_err());
        drop(app);
        assert!(outcome
            .replace(Ok(()))
            .unwrap_err()
            .to_string()
            .contains("regular media file"));
    }

    #[test]
    fn cleanup_failure_is_not_lost_when_an_engine_failure_already_exists() {
        let error = with_cleanup(
            Err(anyhow::anyhow!("engine failure")),
            Err(anyhow::anyhow!("cleanup failure")),
        )
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("engine failure"));
        assert!(diagnostic.contains("cleanup failure"));
        assert!(with_cleanup(Ok(()), Err(anyhow::anyhow!("cleanup failure"))).is_err());
    }
}
