use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use image::{ImageDecoder, ImageReader, RgbaImage, imageops};

use crate::{
    project::{Media, MediaKind},
    scene::SceneFrame,
};

pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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

/// Decoded frame layout. Only video can be decoded to YUV planes; images and
/// scenes are consumed directly by the GPU and shared-memory paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    Bgra,
    Yuv420,
}

/// One tightly packed 8-bit I420 frame, normalized to BT.709 limited range
/// (nominal Y: 16..235, U/V: 16..240, neutral chroma: 128) for the video shader.
/// Chroma planes cover `ceil(width / 2)` columns and `ceil(height / 2)` rows.
/// FFmpeg converts the matrix/range; untagged inputs use its automatic defaults,
/// which cannot recover missing or incorrect source color information. This is
/// not transfer-function/primaries conversion or HDR tone mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Bgra(Vec<u8>),
    Yuv420(Yuv420Frame),
}

impl Frame {
    pub fn len(&self) -> usize {
        match self {
            Frame::Bgra(pixels) => pixels.len(),
            Frame::Yuv420(frame) => frame.y.len() + frame.u.len() + frame.v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Extract the shared-memory pixel payload of a BGRA decode.
    pub fn into_bgra(self) -> Result<Vec<u8>> {
        match self {
            Frame::Bgra(pixels) => Ok(pixels),
            Frame::Yuv420(_) => {
                anyhow::bail!("decoder returned YUV planes where BGRA pixels are required")
            }
        }
    }
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

/// Byte length of one tightly packed I420 frame at the given output size.
pub fn yuv420_len(width: u32, height: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "frame dimensions must be nonzero");
    let luma = u64::from(width)
        .checked_mul(u64::from(height))
        .context("frame size overflow")?;
    let chroma = u64::from(width.div_ceil(2))
        .checked_mul(u64::from(height.div_ceil(2)))
        .context("frame size overflow")?;
    let bytes = luma
        .checked_add(chroma * 2)
        .context("frame size overflow")?;
    ensure!(
        bytes <= MAX_FRAME_BYTES as u64,
        "frame exceeds 64 MiB shared-memory budget; use a smaller output resolution or scale"
    );
    Ok(bytes as usize)
}

/// Split one tightly packed, already BT.709 limited-range I420 payload into planes.
pub fn yuv420_frame(width: u32, height: u32, bytes: Vec<u8>) -> Result<Yuv420Frame> {
    let expected = yuv420_len(width, height)?;
    ensure!(
        bytes.len() == expected,
        "I420 frame has {} bytes; {width}x{height} requires {expected}",
        bytes.len()
    );
    let luma = (width as usize) * (height as usize);
    let chroma = (width.div_ceil(2) as usize) * (height.div_ceil(2) as usize);
    let mut bytes = bytes;
    let v = bytes.split_off(luma + chroma);
    let u = bytes.split_off(luma);
    Ok(Yuv420Frame {
        width,
        height,
        y: bytes,
        u,
        v,
    })
}

/// Decode one still image with decoder and expanded RGBA frame allocation limits.
pub fn decode_image(path: &Path) -> Result<RgbaImage> {
    let mut input = ImageReader::open(path)
        .context("cannot open image")?
        .with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_FRAME_BYTES as u64);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    input.limits(limits.clone());
    let mut decoder = input.into_decoder().context("image decoding failed")?;
    let (width, height) = decoder.dimensions();
    // Decoder limits cover the source representation, not its RGBA expansion.
    frame_len(width, height).context("decoded RGBA image exceeds frame budget")?;
    limits
        .reserve(decoder.total_bytes())
        .context("image decoding failed")?;
    decoder
        .set_limits(limits)
        .context("image decoding failed")?;
    image::DynamicImage::from_decoder(decoder)
        .context("image decoding failed")
        .map(|image| image.into_rgba8())
}

#[derive(Default)]
struct Latest {
    frame: Option<Frame>,
    error: Option<String>,
}

pub struct MediaWorker {
    latest: Arc<Mutex<Latest>>,
    stop: Arc<AtomicBool>,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    scene: Option<SceneFrame>,
}

impl MediaWorker {
    /// Start a decoder for a shared-memory playback target (BGRA pixels).
    pub fn start(
        media: &Media,
        width: u32,
        height: u32,
        options: &PlaybackOptions,
    ) -> Result<Self> {
        Self::start_with(media, width, height, options, FrameFormat::Bgra)
    }

    /// Start a decoder for a GPU playback target (YUV planes for video).
    ///
    /// YUV output exists only for video; images and scenes are consumed
    /// directly from their decoded sources by the GPU paths.
    pub fn start_yuv(
        media: &Media,
        width: u32,
        height: u32,
        options: &PlaybackOptions,
    ) -> Result<Self> {
        Self::start_with(media, width, height, options, FrameFormat::Yuv420)
    }

    fn start_with(
        media: &Media,
        width: u32,
        height: u32,
        options: &PlaybackOptions,
        format: FrameFormat,
    ) -> Result<Self> {
        let bytes = match format {
            FrameFormat::Bgra => frame_len(width, height)?,
            FrameFormat::Yuv420 => yuv420_len(width, height)?,
        };
        ensure!(
            (1..=60).contains(&options.fps),
            "FPS must be between 1 and 60"
        );
        if format == FrameFormat::Yuv420 {
            ensure!(
                media.kind == MediaKind::Video,
                "YUV decoding applies to video only; {:?} keeps its direct source",
                media.kind
            );
        }
        let latest = Arc::new(Mutex::new(Latest::default()));
        let stop = Arc::new(AtomicBool::new(false));
        if media.kind == MediaKind::Scene {
            let scene = media
                .scene
                .clone()
                .context("scene selection is missing its validated renderer")?;
            let frame = scene.start_frame(width, height, options.fit)?;
            return Ok(Self {
                latest,
                stop,
                child: None,
                reader: None,
                scene: Some(frame),
            });
        }
        if media.kind == MediaKind::Image {
            let pixels = fit_image(&decode_image(&media.path)?, width, height, options.fit)?;
            latest.lock().unwrap().frame = Some(Frame::Bgra(pixels));
            return Ok(Self {
                latest,
                stop,
                child: None,
                reader: None,
                scene: None,
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
                &video_filter(width, height, options, format),
                "-pix_fmt",
                match format {
                    FrameFormat::Bgra => "bgra",
                    FrameFormat::Yuv420 => "yuv420p",
                },
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
            scene: None,
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
                let mut bytes = vec![0; bytes];
                if let Err(error) = stdout.read_exact(&mut bytes) {
                    if !cancelled.load(Ordering::Relaxed) {
                        state.lock().unwrap().error = Some(format!("FFmpeg did not produce a complete frame: {error}; check its diagnostic above"));
                    }
                    break;
                }
                let frame = match format {
                    FrameFormat::Bgra => Frame::Bgra(bytes),
                    FrameFormat::Yuv420 => match yuv420_frame(width, height, bytes) {
                        Ok(frame) => Frame::Yuv420(frame),
                        Err(error) => {
                            state.lock().unwrap().error = Some(format!("{error:#}"));
                            break;
                        }
                    },
                };
                // Keep only the latest frame, never an unbounded decoder queue.
                state.lock().unwrap().frame = Some(frame);
            }
        }).context("cannot start video reader")?);
        Ok(worker)
    }

    pub fn take_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(frame) = &self.scene {
            return Ok(Some(Frame::Bgra(frame.render_frame(frame.elapsed())?)));
        }
        let mut latest = self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("frame decoder state was poisoned"))?;
        if let Some(error) = latest.error.take() {
            bail!("{error}");
        }
        Ok(latest.frame.take())
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
        self.scene = None;
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

fn video_filter(width: u32, height: u32, options: &PlaybackOptions, format: FrameFormat) -> String {
    let fps = options.fps;
    // Convert samples at the scale boundary, rather than only tagging raw output.
    let color = match format {
        FrameFormat::Bgra => "",
        FrameFormat::Yuv420 => ":out_color_matrix=bt709:out_range=tv",
    };
    match options.fit {
        Fit::Cover => format!(
            "fps={fps},crop=w='max(1,min(iw,ih*{width}/{height}))':h='max(1,min(ih,iw*{height}/{width}))':exact=1,scale={width}:{height}{color},setsar=1"
        ),
        Fit::Contain => format!(
            "fps={fps},scale={width}:{height}:force_original_aspect_ratio=decrease{color},pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"
        ),
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
    fn i420_sizes_use_ceil_chroma_and_split_planes_exactly() {
        assert_eq!(yuv420_len(4, 4).unwrap(), 16 + 2 * 4);
        assert_eq!(yuv420_len(3, 3).unwrap(), 9 + 2 * 4);
        assert!(yuv420_len(0, 1).is_err());
        assert!(yuv420_len(u32::MAX, u32::MAX).is_err());
        let bytes: Vec<u8> = (0..yuv420_len(3, 3).unwrap() as u8).collect();
        let frame = yuv420_frame(3, 3, bytes.clone()).unwrap();
        assert_eq!((frame.width, frame.height), (3, 3));
        assert_eq!(frame.y, bytes[..9]);
        assert_eq!(frame.u, bytes[9..13]);
        assert_eq!(frame.v, bytes[13..]);
        assert!(yuv420_frame(3, 3, bytes[..16].to_vec()).is_err());
    }

    #[test]
    fn yuv_frames_are_rejected_where_bgra_pixels_are_required() {
        let frame = yuv420_frame(2, 2, vec![0; yuv420_len(2, 2).unwrap()]).unwrap();
        assert_eq!(Frame::Yuv420(frame.clone()).len(), 6);
        assert!(Frame::Yuv420(frame).into_bgra().is_err());
        assert_eq!(Frame::Bgra(vec![1, 2, 3, 4]).into_bgra().unwrap().len(), 4);
    }

    #[test]
    fn video_filters_fill_or_fit_the_exact_output_size() {
        let options = |fit| PlaybackOptions {
            fps: 24,
            fit,
            ffmpeg: "ffmpeg".into(),
            duration: None,
        };
        for (format, color) in [
            (FrameFormat::Bgra, ""),
            (FrameFormat::Yuv420, ":out_color_matrix=bt709:out_range=tv"),
        ] {
            let cover = video_filter(1920, 1080, &options(Fit::Cover), format);
            assert_eq!(
                cover,
                format!(
                    "fps=24,crop=w='max(1,min(iw,ih*1920/1080))':h='max(1,min(ih,iw*1080/1920))':exact=1,scale=1920:1080{color},setsar=1"
                )
            );
            let contain = video_filter(1920, 1080, &options(Fit::Contain), format);
            assert_eq!(
                contain,
                format!(
                    "fps=24,scale=1920:1080:force_original_aspect_ratio=decrease{color},pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"
                )
            );
        }
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
        assert_eq!(
            worker
                .take_frame()
                .unwrap()
                .unwrap()
                .into_bgra()
                .unwrap()
                .len(),
            16
        );
        assert!(worker.take_frame().unwrap().is_none());
        let error = match MediaWorker::start_yuv(&selected, 2, 2, &options) {
            Ok(_) => panic!("YUV output must not apply to still images"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("video only"),
            "YUV output must not apply to still images"
        );
        worker.shutdown().unwrap();
        std::fs::write(path, b"invalid").unwrap();
        assert!(MediaWorker::start(&selected, 2, 2, &options).is_err());
    }

    #[test]
    fn rejects_oversized_grayscale_and_rgb_headers_before_pixel_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.png");
        for (color, rgba) in [
            (image::ColorType::L8, [17, 17, 17, 255]),
            (image::ColorType::Rgb8, [17, 33, 65, 255]),
        ] {
            image::save_buffer(
                &path,
                &[17, 33, 65][..usize::from(color.bytes_per_pixel())],
                1,
                1,
                color,
            )
            .unwrap();
            assert_eq!(decode_image(&path).unwrap().get_pixel(0, 0).0, rgba);
            let mut bytes = std::fs::read(&path).unwrap();
            assert!(bytes.len() < 1024);
            // Change only the PNG IHDR and its CRC; pixel data stays one pixel.
            bytes[16..20].copy_from_slice(&4097u32.to_be_bytes());
            bytes[20..24].copy_from_slice(&4096u32.to_be_bytes());
            let mut crc = u32::MAX;
            for byte in &bytes[12..29] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
            std::fs::write(&path, bytes).unwrap();

            let decoder = ImageReader::open(&path).unwrap().into_decoder().unwrap();
            assert_eq!(decoder.dimensions(), (4097, 4096));
            assert_eq!(decoder.color_type(), color);
            assert!(decoder.total_bytes() <= MAX_FRAME_BYTES as u64);
            let error = decode_image(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("64 MiB"),
                "{color:?} must fail the RGBA budget check before reading pixels: {error:#}"
            );
        }
    }

    #[test]
    #[ignore = "requires WALLPAPER_TEST_FFMPEG pointing to a real FFmpeg binary"]
    fn video_decoding_and_cleanup_with_real_ffmpeg_when_explicitly_selected() {
        let Some(ffmpeg) = std::env::var_os("WALLPAPER_TEST_FFMPEG") else {
            eprintln!("skipping FFmpeg check without WALLPAPER_TEST_FFMPEG");
            return;
        };
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
                let frame = frame.into_bgra().unwrap();
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

    #[test]
    #[ignore = "requires WALLPAPER_TEST_FFMPEG pointing to a real FFmpeg binary"]
    fn yuv_video_decoding_matches_bgra_output_when_explicitly_selected() {
        let Some(ffmpeg) = std::env::var_os("WALLPAPER_TEST_FFMPEG") else {
            eprintln!("skipping FFmpeg check without WALLPAPER_TEST_FFMPEG");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let colors = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [0, 0, 0],
            [128, 128, 128],
            [192, 96, 32],
            [32, 160, 224],
        ];
        let patches = dir.path().join("patches.png");
        image::RgbImage::from_fn(128, 16, |x, _| image::Rgb(colors[(x / 16) as usize]))
            .save(&patches)
            .unwrap();
        for (matrix, range) in [
            ("bt709", "tv"),
            ("bt709", "pc"),
            ("smpte170m", "tv"),
            ("smpte170m", "pc"),
        ] {
            let path = dir.path().join(format!("{matrix}-{range}.mkv"));
            let status = Command::new(&ffmpeg)
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-loop",
                    "1",
                    "-framerate",
                    "10",
                    "-i",
                ])
                .arg(&patches)
                .args([
                    "-vf",
                    &format!("scale=out_color_matrix={matrix}:out_range={range},format=yuv420p"),
                    "-colorspace",
                    matrix,
                    "-color_range",
                    range,
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
            for fit in [Fit::Cover, Fit::Contain] {
                let options = PlaybackOptions {
                    fps: 10,
                    fit,
                    ffmpeg: ffmpeg.clone().into(),
                    duration: None,
                };
                let mut bgra = MediaWorker::start(&selected, 128, 16, &options).unwrap();
                let mut yuv = MediaWorker::start_yuv(&selected, 128, 16, &options).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut reference = None;
                let mut received = 0;
                while received < 5 && Instant::now() < deadline {
                    if let Some(frame) = bgra.take_frame().unwrap() {
                        reference = Some(frame.into_bgra().unwrap());
                    }
                    if let Some(Frame::Yuv420(frame)) = yuv.take_frame().unwrap() {
                        assert_eq!((frame.width, frame.height), (128, 16));
                        assert_eq!(frame.y.len(), 128 * 16);
                        assert_eq!(frame.u.len(), 64 * 8);
                        assert_eq!(frame.v.len(), 64 * 8);
                        if let Some(reference) = &reference {
                            assert_eq!(reference.len(), 128 * 16 * 4);
                            for (patch, expected) in colors.iter().enumerate() {
                                // Patch centers avoid chroma interpolation across color edges.
                                let x = patch * 16 + 8;
                                let luma = 8 * 128 + x;
                                let chroma = 4 * 64 + x / 2;
                                // Match gpu.rs VIDEO_FRAGMENT, including limited-range offsets.
                                let y = (f32::from(frame.y[luma]) - 16.0) / 219.0;
                                let cb = (f32::from(frame.u[chroma]) - 128.0) / 224.0;
                                let cr = (f32::from(frame.v[chroma]) - 128.0) / 224.0;
                                let rgb = [
                                    y + 1.5748 * cr,
                                    y - 0.1873 * cb - 0.4681 * cr,
                                    y + 1.8556 * cb,
                                ]
                                .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
                                let offset = luma * 4;
                                let bgra_rgb = [
                                    reference[offset + 2],
                                    reference[offset + 1],
                                    reference[offset],
                                ];
                                assert_eq!(reference[offset + 3], 255);
                                for channel in 0..3 {
                                    assert!(
                                        bgra_rgb[channel].abs_diff(expected[channel]) <= 5,
                                        "{matrix}/{range} {fit:?} patch {patch}: BGRA {bgra_rgb:?} != known {expected:?}"
                                    );
                                    assert!(
                                        rgb[channel].abs_diff(bgra_rgb[channel]) <= 5,
                                        "{matrix}/{range} {fit:?} patch {patch}: shader RGB {rgb:?} != BGRA {bgra_rgb:?}"
                                    );
                                }
                            }
                            received += 1;
                        }
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    received >= 5,
                    "{matrix}/{range} {fit:?}: YUV and BGRA must match beyond three source frames"
                );
                yuv.shutdown().unwrap();
                bgra.shutdown().unwrap();
                assert!(yuv.child.is_none() && yuv.reader.is_none());
                assert!(bgra.child.is_none() && bgra.reader.is_none());
            }
        }
    }
}
