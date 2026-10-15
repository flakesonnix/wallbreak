use std::{
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use clap::ValueEnum;
use image::{imageops, ImageReader, RgbaImage};

use crate::project::{Media, MediaKind};

pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Fit {
    Cover,
    Contain,
}

#[derive(Clone)]
pub struct PlaybackOptions {
    pub fps: u32,
    pub fit: Fit,
    pub ffmpeg: PathBuf,
    pub duration: Option<Duration>,
}

pub fn frame_len(width: u32, height: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "frame dimensions must be nonzero");
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4))
        .context("frame size overflow")?;
    ensure!(
        bytes <= MAX_FRAME_BYTES as u64,
        "frame exceeds 64 MiB shared-memory budget; use a smaller output resolution or scale"
    );
    Ok(bytes as usize)
}

#[derive(Default)]
struct Latest {
    pixels: Option<Vec<u8>>,
    error: Option<String>,
}

pub struct MediaWorker {
    latest: Arc<Mutex<Latest>>,
    stop: Arc<AtomicBool>,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
}

impl MediaWorker {
    pub fn start(
        media: &Media,
        width: u32,
        height: u32,
        options: &PlaybackOptions,
    ) -> Result<Self> {
        let bytes = frame_len(width, height)?;
        ensure!(
            (1..=60).contains(&options.fps),
            "FPS must be between 1 and 60"
        );
        let latest = Arc::new(Mutex::new(Latest::default()));
        let stop = Arc::new(AtomicBool::new(false));
        if media.kind == MediaKind::Image {
            let mut input = ImageReader::open(&media.path)
                .context("cannot open image")?
                .with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(MAX_FRAME_BYTES as u64);
            limits.max_image_width = Some(16_384);
            limits.max_image_height = Some(16_384);
            input.limits(limits);
            let decoded = input
                .decode()
                .context("image decoding failed")?
                .into_rgba8();
            let pixels = fit_image(&decoded, width, height, options.fit)?;
            latest.lock().unwrap().pixels = Some(pixels);
            return Ok(Self {
                latest,
                stop,
                child: None,
                reader: None,
            });
        }

        let mut command = Command::new(&options.ffmpeg);
        command
            .args([
                "-nostdin",
                "-v",
                "error",
                "-max_alloc",
                "67108864",
                "-threads",
                "2",
                "-filter_threads",
                "1",
                "-protocol_whitelist",
                "file,pipe",
                "-stream_loop",
                "-1",
                "-re",
                "-i",
            ])
            .arg(&media.path)
            .args([
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-dn",
                "-vf",
                &video_filter(width, height, options),
                "-pix_fmt",
                "bgra",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let child = command.spawn().with_context(|| {
            format!(
                "cannot start {}; provide FFmpeg on PATH, --ffmpeg, or use nix develop",
                options.ffmpeg.display()
            )
        })?;
        let mut worker = Self {
            latest,
            stop,
            child: Some(child),
            reader: None,
        };
        let mut stdout = worker
            .child
            .as_mut()
            .unwrap()
            .stdout
            .take()
            .expect("piped video output");
        let state = worker.latest.clone();
        let cancelled = worker.stop.clone();
        worker.reader = Some(thread::Builder::new().name("video-decoder".into()).spawn(move || {
            while !cancelled.load(Ordering::Relaxed) {
                let mut frame = vec![0; bytes];
                if let Err(error) = stdout.read_exact(&mut frame) {
                    if !cancelled.load(Ordering::Relaxed) {
                        state.lock().unwrap().error = Some(format!("FFmpeg did not produce a complete frame: {error}; check its diagnostic above"));
                    }
                    break;
                }
                // Keep only the latest frame, never an unbounded decoder queue.
                state.lock().unwrap().pixels = Some(frame);
            }
        }).context("cannot start video reader")?);
        Ok(worker)
    }

    pub fn take_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let mut latest = self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("frame decoder state was poisoned"))?;
        if let Some(error) = latest.error.take() {
            bail!("{error}");
        }
        Ok(latest.pixels.take())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = &mut self.child {
            if child
                .try_wait()
                .context("cannot inspect FFmpeg exit")?
                .is_none()
            {
                child
                    .kill()
                    .context("cannot terminate owned FFmpeg process")?;
            }
            child.wait().context("cannot reap FFmpeg process")?;
        }
        self.child = None;
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("video reader panicked"))?;
        }
        Ok(())
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("decoder cleanup failed: {error:#}");
        }
    }
}

fn video_filter(width: u32, height: u32, options: &PlaybackOptions) -> String {
    let fps = options.fps;
    match options.fit {
        Fit::Cover => format!("fps={fps},crop=w='max(1,min(iw,ih*{width}/{height}))':h='max(1,min(ih,iw*{height}/{width}))':exact=1,scale={width}:{height},setsar=1"),
        Fit::Contain => format!("fps={fps},scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"),
    }
}

pub fn fit_image(source: &RgbaImage, width: u32, height: u32, fit: Fit) -> Result<Vec<u8>> {
    let bytes = frame_len(width, height)?;
    let (sw, sh) = source.dimensions();
    ensure!(sw > 0 && sh > 0, "source image has zero dimensions");
    let wide = u64::from(sw) * u64::from(height) > u64::from(sh) * u64::from(width);
    let (resized, left, top) = match fit {
        Fit::Cover => {
            // Crop before resizing: extreme aspect ratios must not create huge intermediates.
            let (cw, ch) = if wide {
                (
                    ((u64::from(sh) * u64::from(width)) / u64::from(height)).max(1) as u32,
                    sh,
                )
            } else {
                (
                    sw,
                    ((u64::from(sw) * u64::from(height)) / u64::from(width)).max(1) as u32,
                )
            };
            let crop = imageops::crop_imm(source, (sw - cw) / 2, (sh - ch) / 2, cw, ch);
            (
                imageops::resize(&*crop, width, height, imageops::FilterType::Triangle),
                0,
                0,
            )
        }
        Fit::Contain => {
            let (dw, dh) = if wide {
                (
                    width,
                    ((u64::from(sh) * u64::from(width)) / u64::from(sw)).max(1) as u32,
                )
            } else {
                (
                    ((u64::from(sw) * u64::from(height)) / u64::from(sh)).max(1) as u32,
                    height,
                )
            };
            (
                imageops::resize(source, dw, dh, imageops::FilterType::Triangle),
                (width - dw) / 2,
                (height - dh) / 2,
            )
        }
    };
    let mut pixels = vec![0; bytes];
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    for (x, y, pixel) in resized.enumerate_pixels() {
        let offset = (((y + top) as usize * width as usize) + (x + left) as usize) * 4;
        let alpha = u16::from(pixel[3]);
        pixels[offset..offset + 4].copy_from_slice(&[
            (u16::from(pixel[2]) * alpha / 255) as u8,
            (u16::from(pixel[1]) * alpha / 255) as u8,
            (u16::from(pixel[0]) * alpha / 255) as u8,
            255,
        ]);
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::time::Instant;

    #[test]
    fn rejects_invalid_or_unbounded_frames_before_allocation() {
        assert!(frame_len(0, 1).is_err());
        assert!(frame_len(u32::MAX, u32::MAX).is_err());
        assert!(frame_len(8192, 8192).is_err());
        assert_eq!(frame_len(1920, 1080).unwrap(), 1920 * 1080 * 4);
    }

    #[test]
    fn letterboxes_and_uses_opaque_bgra() {
        let source = RgbaImage::from_pixel(4, 2, Rgba([255, 0, 0, 255]));
        let frame = fit_image(&source, 4, 4, Fit::Contain).unwrap();
        assert_eq!(&frame[..4], &[0, 0, 0, 255]);
        assert_eq!(&frame[16..20], &[0, 0, 255, 255]);
        assert_eq!(&frame[48..52], &[0, 0, 0, 255]);
    }

    #[test]
    fn crops_without_excessive_intermediate_and_composites_alpha() {
        let source = RgbaImage::from_pixel(1, 4096, Rgba([200, 100, 50, 128]));
        let frame = fit_image(&source, 4096, 1, Fit::Cover).unwrap();
        assert_eq!(frame.len(), 4096 * 4);
        assert_eq!(&frame[..4], &[25, 50, 100, 255]);
    }

    #[test]
    fn image_worker_returns_one_frame_and_rejects_malformed_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.png");
        RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let selected = crate::project::resolve(&path, false).unwrap();
        let options = PlaybackOptions {
            fps: 30,
            fit: Fit::Cover,
            ffmpeg: "missing-ffmpeg".into(),
            duration: None,
        };
        let mut worker = MediaWorker::start(&selected, 2, 2, &options).unwrap();
        assert_eq!(worker.take_frame().unwrap().unwrap().len(), 16);
        assert!(worker.take_frame().unwrap().is_none());
        worker.shutdown().unwrap();
        std::fs::write(path, b"invalid").unwrap();
        assert!(MediaWorker::start(&selected, 2, 2, &options).is_err());
    }

    #[test]
    #[ignore = "requires WALLPAPER_TEST_FFMPEG pointing to a real FFmpeg binary"]
    fn video_decoding_and_cleanup_with_real_ffmpeg_when_explicitly_selected() {
        let ffmpeg = std::env::var_os("WALLPAPER_TEST_FFMPEG")
            .expect("set WALLPAPER_TEST_FFMPEG to a real FFmpeg binary");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.mkv");
        let status = Command::new(&ffmpeg)
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x16:r=10",
                "-t",
                "0.3",
                "-c:v",
                "ffv1",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let selected = crate::project::resolve(&path, false).unwrap();
        let options = PlaybackOptions {
            fps: 10,
            fit: Fit::Contain,
            ffmpeg: ffmpeg.into(),
            duration: None,
        };
        let mut worker = MediaWorker::start(&selected, 16, 16, &options).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = 0;
        while received < 5 && Instant::now() < deadline {
            if let Some(frame) = worker.take_frame().unwrap() {
                assert_eq!(frame.len(), 1024);
                assert!(frame[2] > 200);
                received += 1;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            received >= 5,
            "video must decode and loop beyond its three source frames"
        );
        worker.shutdown().unwrap();
        assert!(worker.child.is_none());
        assert!(worker.reader.is_none());
    }
}
