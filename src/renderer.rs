//! Ordinary, bridge-owned Wayland source window for GNOME animation clones.
//!
//! Scene, image, and video content is uploaded and drawn on the GPU through the
//! window's own glow context; only video frames are still decoded on the CPU.

use std::{
    cell::RefCell,
    os::{fd::BorrowedFd, unix::fs::FileTypeExt},
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use eframe::{egui, egui_glow, glow};

use crate::{
    gpu::{Clock, GpuImage, GpuScene, GpuVideo, Renderer},
    media::{decode_image, frame_len, Fit, Frame, MediaWorker, PlaybackOptions, Yuv420Frame},
    project,
    scene::Scene,
};

pub const APP_ID: &str = "io.github.flakesonnix.wallbreak.Renderer";
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL: Duration = Duration::from_millis(100);
const FIRST_FRAME_POLL: Duration = Duration::from_millis(5);
const VIDEO_POLL: Duration = Duration::from_millis(2);

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
/// FFmpeg is stopped and reaped on exit; in-progress image/scene loading must
/// finish before its preparation worker can be joined.
pub fn run(args: Args, close: Arc<AtomicBool>) -> Result<()> {
    args.validate()?;
    require_owned_socket()?;
    let options = args.native_options();
    let outcome = Rc::new(RefCell::new(Ok(())));
    let mut app = Animation::start(args, close, outcome.clone())?;
    let result = match app.first_frame(FIRST_FRAME_TIMEOUT) {
        Ok(Some(source)) if !app.close.load(Ordering::Relaxed) => {
            app.source = Some(Arc::new(source));
            eframe::run_native(APP_ID, options, Box::new(move |_cc| Ok(Box::new(app)))).map_err(
                |error| anyhow::anyhow!("renderer window/Wayland connection failed: {error}"),
            )
        }
        result => {
            drop(app);
            result.map(|_| ())
        }
    };
    // Every branch has dropped the app before consuming its cleanup outcome.
    with_cleanup(result, outcome.replace(Ok(())))
}

#[derive(Default)]
struct LatestVideo {
    frame: Option<Yuv420Frame>,
    error: Option<String>,
}

/// One resolved source, shared with the paint callback.
enum Source {
    Scene { scene: Arc<Scene>, clock: Clock },
    Image(image::RgbaImage),
    Video { latest: Arc<Mutex<LatestVideo>> },
}

/// A resolved source plus the decoder handle the app must keep joining.
enum Ready {
    Cancelled,
    Content(Source),
    Video {
        latest: Arc<Mutex<LatestVideo>>,
        decoder: JoinHandle<Result<()>>,
    },
}

/// GL objects created inside the paint callback on first use.
#[derive(Default)]
struct GpuState {
    renderer: Option<Renderer>,
    scene: Option<GpuScene>,
    image: Option<GpuImage>,
    video: Option<GpuVideo>,
}

fn draw_content(
    gl: &glow::Context,
    state: &Mutex<GpuState>,
    source: &Source,
    fit: Fit,
    info: egui::PaintCallbackInfo,
) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("GPU state was poisoned"))?;
    if state.renderer.is_none() {
        state.renderer = Some(Renderer::new(gl).context("GPU renderer initialization failed")?);
    }
    let GpuState {
        renderer,
        scene,
        image,
        video,
    } = &mut *state;
    let renderer = renderer.as_ref().unwrap();
    let viewport = info.viewport_in_pixels();
    let width = u32::try_from(viewport.width_px).context("paint viewport has no width")?;
    let height = u32::try_from(viewport.height_px).context("paint viewport has no height")?;
    match source {
        Source::Scene {
            scene: source_scene,
            clock,
        } => {
            if scene.is_none() {
                *scene =
                    Some(GpuScene::new(gl, source_scene).context("scene texture upload failed")?);
            }
            renderer
                .draw_scene(
                    gl,
                    scene.as_ref().unwrap(),
                    clock.elapsed(),
                    width,
                    height,
                    fit,
                )
                .context("scene draw failed")?;
        }
        Source::Image(source_image) => {
            if image.is_none() {
                *image =
                    Some(GpuImage::new(gl, source_image).context("image texture upload failed")?);
            }
            renderer
                .draw_image(gl, image.as_ref().unwrap(), width, height, fit)
                .context("image draw failed")?;
        }
        Source::Video { latest } => {
            if video.is_none() {
                *video = Some(GpuVideo::new(gl).context("video texture setup failed")?);
            }
            let frame = latest
                .lock()
                .map_err(|_| anyhow::anyhow!("video frame state was poisoned"))?
                .frame
                .take();
            if let Some(frame) = frame {
                video
                    .as_mut()
                    .unwrap()
                    .upload(gl, &frame)
                    .context("video frame upload failed")?;
            }
            renderer
                .draw_video(gl, video.as_ref().unwrap())
                .context("video draw failed")?;
        }
    }
    Ok(())
}

struct Animation {
    close: Arc<AtomicBool>,
    preparation: Option<JoinHandle<Result<Ready>>>,
    decoder: Option<JoinHandle<Result<()>>>,
    ready: Receiver<()>,
    source: Option<Arc<Source>>,
    gpu: Arc<Mutex<GpuState>>,
    gpu_error: Arc<Mutex<Option<String>>>,
    fit: Fit,
    interval: Duration,
    next_frame: Instant,
    outcome: Rc<RefCell<Result<()>>>,
}

impl Animation {
    fn start(args: Args, close: Arc<AtomicBool>, outcome: Rc<RefCell<Result<()>>>) -> Result<Self> {
        args.validate()?;
        let interval = Duration::from_secs_f64(1.0 / f64::from(args.fps));
        let (send_ready, ready) = mpsc::sync_channel(1);
        let cancelled = close.clone();
        let preparation = thread::Builder::new()
            .name("animation-source".into())
            .spawn(move || {
                let result = (|| -> Result<Ready> {
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(Ready::Cancelled);
                    }
                    ensure!(
                        args.source.is_file(),
                        "--source must be a regular media file, project.json or scene.json"
                    );
                    // Re-resolve the selected source, not a substituted project or preview.
                    let media = project::resolve(&args.source, false)?;
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok(Ready::Cancelled);
                    }
                    match media.kind {
                        project::MediaKind::Scene => {
                            let scene = media
                                .scene
                                .clone()
                                .context("scene selection is missing its validated renderer")?;
                            Ok(Ready::Content(Source::Scene {
                                scene: Arc::new(scene),
                                clock: Clock::start(),
                            }))
                        }
                        project::MediaKind::Image => {
                            Ok(Ready::Content(Source::Image(decode_image(&media.path)?)))
                        }
                        _ => {
                            let options = PlaybackOptions {
                                fps: args.fps,
                                fit: args.fit,
                                ffmpeg: args.ffmpeg.clone(),
                                duration: None,
                            };
                            let (latest, decoder) = start_video_decoder(
                                media,
                                args.width,
                                args.height,
                                options,
                                cancelled.clone(),
                            )?;
                            Ok(Ready::Video { latest, decoder })
                        }
                    }
                })();
                // Only signal readiness here. The join handle remains the sole
                // owner of the result, including any decoder that needs joining.
                let _ = send_ready.send(());
                result
            })
            .context("cannot start animation source worker")?;
        Ok(Self {
            close,
            preparation: Some(preparation),
            decoder: None,
            ready,
            source: None,
            gpu: Arc::new(Mutex::new(GpuState::default())),
            gpu_error: Arc::new(Mutex::new(None)),
            fit: args.fit,
            interval,
            next_frame: Instant::now(),
            outcome,
        })
    }

    fn first_frame(&mut self, timeout: Duration) -> Result<Option<Source>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.close.load(Ordering::Relaxed) {
                return Ok(None);
            }
            ensure!(
                Instant::now() < deadline,
                "decoder produced no complete first frame within {timeout:?}"
            );
            match self.ready.recv_timeout(
                FIRST_FRAME_POLL.min(deadline.saturating_duration_since(Instant::now())),
            ) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    let ready = self
                        .preparation
                        .take()
                        .context("animation source worker is missing")?
                        .join()
                        .map_err(|_| anyhow::anyhow!("animation source worker panicked"))??;
                    let source = match ready {
                        Ready::Cancelled => return Ok(None),
                        Ready::Content(source) => source,
                        Ready::Video { latest, decoder } => {
                            self.decoder = Some(decoder);
                            wait_for_first_video_frame(&latest, &self.close, deadline, timeout)?;
                            Source::Video { latest }
                        }
                    };
                    if self.close.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "decoder produced no complete first frame within {timeout:?}"
                    );
                    return Ok(Some(source));
                }
            }
        }
    }

    fn add_callback(&self, ctx: &egui::Context) {
        let Some(source) = self.source.clone() else {
            return;
        };
        let gpu = self.gpu.clone();
        let gpu_error = self.gpu_error.clone();
        let fit = self.fit;
        ctx.layer_painter(egui::LayerId::background())
            .add(egui::PaintCallback {
                rect: ctx.screen_rect(),
                callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                    let gl = painter.gl();
                    if let Err(error) = draw_content(gl, &gpu, &source, fit, info) {
                        if let Ok(mut slot) = gpu_error.lock() {
                            if slot.is_none() {
                                *slot = Some(format!("{error:#}"));
                            }
                        }
                    }
                })),
            });
    }

    fn show(&mut self, ctx: &egui::Context) -> Result<()> {
        if self.close.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return Ok(());
        }
        if let Some(error) = self
            .gpu_error
            .lock()
            .map_err(|_| anyhow::anyhow!("GPU state was poisoned"))?
            .take()
        {
            bail!("{error}");
        }
        if let Some(Source::Video { latest }) = self.source.as_deref() {
            if let Some(error) = latest
                .lock()
                .map_err(|_| anyhow::anyhow!("video frame state was poisoned"))?
                .error
                .take()
            {
                bail!("{error}");
            }
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = now
                .checked_add(self.interval)
                .context("frame clock overflow")?;
        }
        self.add_callback(ctx);
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

    /// One eframe tick: paint, or record the failure and close the window.
    fn tick(&mut self, ctx: &egui::Context) {
        if let Err(error) = self.show(ctx) {
            if !self.close.load(Ordering::Relaxed) {
                *self.outcome.borrow_mut() = Err(error);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Latest decoded frame shared with the paint callback, plus the worker handle
/// teardown joins.
type Decoder = (Arc<Mutex<LatestVideo>>, JoinHandle<Result<()>>);

fn start_video_decoder(
    media: project::Media,
    width: u32,
    height: u32,
    options: PlaybackOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<Decoder> {
    let latest = Arc::new(Mutex::new(LatestVideo::default()));
    let state = latest.clone();
    let decoder = thread::Builder::new()
        .name("animation-frames".into())
        .spawn(move || {
            let mut worker = match MediaWorker::start_yuv(&media, width, height, &options) {
                Ok(worker) => worker,
                Err(error) => {
                    publish_decoder_error(&state, &error);
                    return Err(error);
                }
            };
            let result = (|| {
                while !cancelled.load(Ordering::Relaxed) {
                    match worker.take_frame()? {
                        Some(Frame::Yuv420(frame)) => {
                            if let Ok(mut slot) = state.lock() {
                                slot.frame = Some(frame);
                            }
                        }
                        Some(Frame::Bgra(_)) => {
                            bail!("YUV decoder returned BGRA pixels")
                        }
                        None => thread::sleep(VIDEO_POLL),
                    }
                }
                Ok(())
            })();
            let cleanup = worker.shutdown();
            if let Err(error) = &result {
                publish_decoder_error(&state, error);
            }
            with_cleanup(result, cleanup)
        })
        .context("cannot start animation frame worker")?;
    Ok((latest, decoder))
}

/// Publish the first decoder failure for the UI thread, which closes the
/// window with it instead of silently freezing the last frame.
fn publish_decoder_error(state: &Arc<Mutex<LatestVideo>>, error: &anyhow::Error) {
    if let Ok(mut slot) = state.lock() {
        if slot.error.is_none() {
            slot.error = Some(format!("{error:#}"));
        }
    }
}

/// Block until the decoder produced one complete frame, respecting cancellation.
fn wait_for_first_video_frame(
    latest: &Arc<Mutex<LatestVideo>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Instant,
    timeout: Duration,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "decoder produced no complete first frame within {timeout:?}"
        );
        let mut slot = latest
            .lock()
            .map_err(|_| anyhow::anyhow!("video frame state was poisoned"))?;
        if let Some(error) = slot.error.take() {
            bail!("{error}");
        }
        if slot.frame.is_some() {
            return Ok(());
        }
        drop(slot);
        thread::sleep(FIRST_FRAME_POLL);
    }
}

impl eframe::App for Animation {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn on_exit(&mut self, gl: Option<&glow::Context>) {
        if let Some(gl) = gl {
            let mut gpu = self.gpu.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(scene) = gpu.scene.take() {
                scene.destroy(gl);
            }
            if let Some(image) = gpu.image.take() {
                image.destroy(gl);
            }
            if let Some(video) = gpu.video.take() {
                video.destroy(gl);
            }
            if let Some(renderer) = gpu.renderer.take() {
                renderer.destroy(gl);
            }
        }
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        self.close.store(true, Ordering::Relaxed);
        if let Some(preparation) = self.preparation.take() {
            let cleanup = preparation
                .join()
                .map_err(|_| anyhow::anyhow!("animation source worker panicked"))
                .and_then(|result| result)
                .map(|ready| {
                    if let Ready::Video { decoder, .. } = ready {
                        self.decoder = Some(decoder);
                    }
                });
            let previous = self.outcome.replace(Ok(()));
            *self.outcome.borrow_mut() = with_cleanup(previous, cleanup);
        }
        if let Some(decoder) = self.decoder.take() {
            let cleanup = decoder
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
    fn image_source_paints_one_full_window_callback_and_keeps_scheduling() {
        let (_directory, mut app, outcome) = image_app();
        let source = app.first_frame(Duration::from_secs(5)).unwrap().unwrap();
        assert!(matches!(source, Source::Image(_)));
        app.source = Some(Arc::new(source));
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| app.show(ctx).unwrap());
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let callback = primitives
            .iter()
            .find_map(|primitive| match &primitive.primitive {
                egui::epaint::Primitive::Callback(callback) => Some(callback),
                _ => None,
            })
            .expect("the GPU draw must be one paint callback");
        assert_eq!(
            callback.rect,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0))
        );
        // The image source has no second frame. Polling must keep scheduling.
        thread::sleep(app.interval);
        for _ in 0..5 {
            let _ = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
        }
        let output = ctx.run(Default::default(), |ctx| app.show(ctx).unwrap());
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
    fn gpu_failures_close_the_window_with_their_diagnostic() {
        let (_directory, mut app, outcome) = image_app();
        let source = app.first_frame(Duration::from_secs(5)).unwrap().unwrap();
        app.source = Some(Arc::new(source));
        *app.gpu_error.lock().unwrap() = Some("scene draw failed: test".into());
        let ctx = egui::Context::default();
        let output = ctx.run(Default::default(), |ctx| app.tick(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close));
        drop(app);
        assert!(outcome
            .replace(Ok(()))
            .unwrap_err()
            .to_string()
            .contains("scene draw failed: test"));
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
        let source = app.first_frame(Duration::from_secs(5)).unwrap().unwrap();
        app.source = Some(Arc::new(source));
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
        let app =
            Animation::start(args(), Arc::new(AtomicBool::new(false)), outcome.clone()).unwrap();
        app.ready.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(app);
        assert!(outcome
            .replace(Ok(()))
            .unwrap_err()
            .to_string()
            .contains("regular media file"));
    }

    #[test]
    fn source_preparation_error_is_returned_once() {
        let outcome = Rc::new(RefCell::new(Ok(())));
        let mut app =
            Animation::start(args(), Arc::new(AtomicBool::new(false)), outcome.clone()).unwrap();
        let error = match app.first_frame(Duration::from_secs(5)) {
            Ok(_) => panic!("a missing source must fail startup"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("regular media file"));
        drop(app);
        outcome.replace(Ok(())).unwrap();
    }

    #[test]
    fn missing_decoder_fails_video_startup_before_window_creation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.mp4");
        std::fs::write(&path, b"not a real video").unwrap();
        let mut args = args();
        args.source = path;
        args.ffmpeg = "missing-ffmpeg-binary".into();
        let outcome = Rc::new(RefCell::new(Ok(())));
        let mut app =
            Animation::start(args, Arc::new(AtomicBool::new(false)), outcome.clone()).unwrap();
        let error = match app.first_frame(Duration::from_secs(5)) {
            Ok(_) => panic!("a missing decoder must fail first-frame startup"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("missing-ffmpeg-binary"),
            "{error:#}"
        );
        drop(app);
        assert!(outcome
            .replace(Ok(()))
            .unwrap_err()
            .to_string()
            .contains("missing-ffmpeg-binary"));
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
