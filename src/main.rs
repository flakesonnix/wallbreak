use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use wallbreak::{
    display, gnome, log, log_debug, log_info,
    media::{Fit, MediaWorker, PlaybackOptions},
    project, steam,
};

#[derive(Parser)]
#[command(
    version,
    about = "Image/video and supported layered scene wallpapers for Wayland"
)]
struct Cli {
    /// Increase log detail: -v info, -vv debug, -vvv trace (default warn; WALLBREAK_LOG overrides).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Resolve direct media, scene.json or a Wallpaper Engine project without playing it.
    Inspect {
        path: PathBuf,
        #[arg(long)]
        preview: bool,
    },
    /// List the current compositor's advertised output names without changing wallpapers.
    Outputs,
    /// List installed Wallpaper Engine projects from local Steam libraries.
    Projects,
    /// Print the optional GNOME 50 extension archive path; never install or enable it.
    GnomeExtension,
    /// Play until Ctrl-C: one layer-shell output, or a static image on all GNOME displays.
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

fn main() {
    let cli = Cli::parse();
    log::init_from_env();
    if cli.verbose > 0 {
        log::set_level(match cli.verbose {
            1 => log::Level::Info,
            2 => log::Level::Debug,
            _ => log::Level::Trace,
        });
    }
    log_debug!(
        "wallbreak {} starting (log level {:?})",
        env!("CARGO_PKG_VERSION"),
        log::level()
    );
    if let Err(error) = run(cli.command) {
        report_error(&error);
        std::process::exit(1);
    }
}

/// Print the full cause chain as plain lines so terminal users see the same
/// context a developer would read from the debug representation.
fn report_error(error: &anyhow::Error) {
    eprintln!("error: {error}");
    for cause in error.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }
}

fn run(command: Action) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, stop.clone())?;
    }
    match command {
        Action::Inspect { path, preview } => {
            let media = project::resolve(&path, preview)?;
            log_info!(
                "resolved {} as {:?} from {}",
                media.path.display(),
                media.kind,
                media.source_path.display()
            );
            for limitation in &media.limitations {
                log_debug!("limitation: {limitation}");
            }
            println!("{}", serde_json::to_string_pretty(&media)?);
        }
        Action::Outputs => println!(
            "{}",
            serde_json::to_string_pretty(&display::list_outputs()?)?,
        ),
        Action::Projects => {
            let entries = steam::scan(&steam::default_roots());
            let supported = entries.iter().filter(|entry| entry.supported).count();
            log_info!(
                "found {} Wallpaper Engine projects, {supported} supported",
                entries.len()
            );
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        Action::GnomeExtension => {
            let path = PathBuf::from(std::env::var_os("WALLBREAK_GNOME_EXTENSION_ZIP")
                .context("GNOME extension archive not configured; use the Nix package to obtain the optional bridge")?);
            ensure!(
                path.is_absolute() && path.is_file(),
                "GNOME extension archive is unavailable"
            );
            println!(
                "{}",
                path.to_str()
                    .context("GNOME extension archive path is not UTF-8")?
            );
        }
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
            log_info!(
                "playing {} ({:?}) at {fps} fps",
                media.path.display(),
                media.kind
            );
            for limitation in &media.limitations {
                eprintln!("Note: {limitation}");
            }
            let options = PlaybackOptions {
                fps,
                fit,
                ffmpeg,
                duration: duration_seconds.map(Duration::from_secs),
            };
            if gnome::is_current() {
                gnome::run(&media, &options, output.as_deref(), stop)?;
            } else {
                display::run(media, options, output, stop)?;
            }
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
            log_info!(
                "checking {} ({:?}) at {width}x{height} for {frames} frame(s)",
                media.path.display(),
                media.kind
            );
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
                    });
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
