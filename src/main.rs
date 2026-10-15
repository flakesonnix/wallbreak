use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, Subcommand};
use wallbreak::{
    display,
    media::{Fit, MediaWorker, PlaybackOptions},
    project,
};

#[derive(Parser)]
#[command(
    version,
    about = "Image/video wallpapers plus one exact animated flag scene for Wayland layer-shell"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Resolve direct media, a video project, or one exact animated flag scene without playing it.
    Inspect {
        path: PathBuf,
        #[arg(long)]
        preview: bool,
    },
    /// List the current compositor's advertised output names without changing wallpapers.
    Outputs,
    /// Show an input-transparent background on one output until Ctrl-C.
    Play {
        path: PathBuf,
        /// Select the project's static preview instead of rendering its scene or web content.
        #[arg(long)]
        preview: bool,
        /// Exact compositor output name; defaults to the first advertised output.
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
        fps: u32,
        #[arg(long, value_enum, default_value_t = Fit::Cover)]
        fit: Fit,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
        /// Automatically unmap the wallpaper and stop its decoder after this many seconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400))]
        duration_seconds: Option<u64>,
    },
    /// Decode bounded frames without connecting to Wayland or modifying the desktop.
    Check {
        path: PathBuf,
        #[arg(long)]
        preview: bool,
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=120))]
        frames: u32,
        #[arg(long, default_value = "ffmpeg")]
        ffmpeg: PathBuf,
    },
}

fn main() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, stop.clone())?;
    }
    match Cli::parse().command {
        Action::Inspect { path, preview } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&project::resolve(&path, preview)?)?
            );
        }
        Action::Outputs => println!(
            "{}",
            serde_json::to_string_pretty(&display::list_outputs()?)?
        ),
        Action::Play {
            path,
            preview,
            output,
            fps,
            fit,
            ffmpeg,
            duration_seconds,
        } => {
            let media = project::resolve(&path, preview)?;
            for limitation in &media.limitations {
                eprintln!("Note: {limitation}");
            }
            let options = PlaybackOptions {
                fps,
                fit,
                ffmpeg,
                duration: duration_seconds.map(Duration::from_secs),
            };
            display::run(media, options, output, stop)?;
        }
        Action::Check {
            path,
            preview,
            width,
            height,
            frames,
            ffmpeg,
        } => {
            let media = project::resolve(&path, preview)?;
            ensure!(
                frames == 1 || !matches!(media.kind, project::MediaKind::Image),
                "static images provide exactly one frame"
            );
            let options = PlaybackOptions {
                fps: 30,
                fit: Fit::Cover,
                ffmpeg,
                duration: None,
            };
            let mut worker = MediaWorker::start(&media, width, height, &options)?;
            let start = Instant::now();
            let result = (|| -> Result<u32> {
                let mut decoded = 0;
                while decoded < frames {
                    ensure!(!stop.load(Ordering::Relaxed), "media validation cancelled");
                    if worker
                        .take_frame()
                        .context("media validation failed")?
                        .is_some()
                    {
                        decoded += 1;
                    }
                    if start.elapsed() > Duration::from_secs(15) {
                        bail!("decoder did not supply {frames} complete frames within 15 seconds");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(decoded)
            })();
            let cleanup = worker.shutdown();
            let decoded = match result {
                Ok(decoded) => decoded,
                Err(error) => {
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup) => {
                            error.context(format!("decoder cleanup also failed: {cleanup:#}"))
                        }
                    })
                }
            };
            cleanup?;
            println!(
                "{}",
                serde_json::json!({"media": media, "width": width, "height": height, "decoded_frames": decoded, "elapsed_ms": start.elapsed().as_millis(), "desktop_modified": false})
            );
        }
    }
    Ok(())
}
