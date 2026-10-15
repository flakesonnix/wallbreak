#![cfg(target_os = "linux")]

use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use image::{Rgba, RgbaImage};
use rustix::process::{kill_process, Pid, Signal};

const BINARY: &str = env!("CARGO_BIN_EXE_wallbreak");

struct OwnedChild(Child);

impl OwnedChild {
    fn wait(&mut self, timeout: Duration) {
        let end = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "owned process failed: {status}");
                return;
            }
            assert!(Instant::now() < end, "owned process did not exit in time");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct Session {
    runtime: PathBuf,
    display: OsString,
    ipc: PathBuf,
    grim: OsString,
}

impl Session {
    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("SWAYSOCK", &self.ipc)
            .env("XDG_CURRENT_DESKTOP", "sway")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY");
        command
    }

    fn screenshot(&self, path: &Path) -> image::RgbaImage {
        let result = self
            .command(&self.grim)
            .args(["-o", "HEADLESS-1"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        image::open(path).unwrap().into_rgba8()
    }
}

#[test]
#[ignore = "requires explicit WALLPAPER_TEST_SWAY, SWAYMSG, GRIM and FFMPEG binaries; creates only an isolated compositor"]
fn presents_image_and_video_resizes_and_stops_in_isolated_sway() {
    for name in [
        "WALLPAPER_TEST_SWAY",
        "WALLPAPER_TEST_SWAYMSG",
        "WALLPAPER_TEST_GRIM",
        "WALLPAPER_TEST_FFMPEG",
    ] {
        if std::env::var_os(name).is_none() {
            eprintln!("skipping headless check without {name}");
            return;
        }
    }
    let required =
        |name| std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to an existing binary"));
    let sway = required("WALLPAPER_TEST_SWAY");
    let swaymsg = required("WALLPAPER_TEST_SWAYMSG");
    let grim = required("WALLPAPER_TEST_GRIM");
    let ffmpeg = required("WALLPAPER_TEST_FFMPEG");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let config = directory.path().join("sway.conf");
    fs::write(&config, "output HEADLESS-1 mode 320x180\n").unwrap();
    let log = directory.path().join("sway.log");
    let mut compositor = OwnedChild(
        Command::new(sway)
            .arg("-c")
            .arg(config)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("SWAYSOCK")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let (display, ipc) = loop {
        let entries: Vec<_> = fs::read_dir(&runtime)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let display = entries.iter().find(|entry| {
            entry.file_name().to_string_lossy().starts_with("wayland-")
                && entry.file_type().unwrap().is_socket()
        });
        let ipc = entries.iter().find(|entry| {
            entry.file_name().to_string_lossy().starts_with("sway-ipc.")
                && entry.file_type().unwrap().is_socket()
        });
        if let (Some(display), Some(ipc)) = (display, ipc) {
            break (display.file_name(), ipc.path());
        }
        assert!(
            compositor.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "isolated Sway failed: {}",
            fs::read_to_string(&log).unwrap()
        );
        thread::sleep(Duration::from_millis(20));
    };
    let session = Session {
        runtime,
        display,
        ipc,
        grim,
    };
    let outputs = session.command(BINARY).arg("outputs").output().unwrap();
    assert!(
        outputs.status.success(),
        "{}",
        String::from_utf8_lossy(&outputs.stderr)
    );
    let outputs: serde_json::Value = serde_json::from_slice(&outputs.stdout).unwrap();
    assert_eq!(outputs[0]["name"], "HEADLESS-1");

    let image = directory.path().join("red.png");
    RgbaImage::from_pixel(16, 16, Rgba([255, 0, 0, 255]))
        .save(&image)
        .unwrap();
    let missing = session
        .command(BINARY)
        .arg("play")
        .arg(&image)
        .args(["--output", "MISSING"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
    let mut player = OwnedChild(
        session
            .command(BINARY)
            .arg("play")
            .arg(&image)
            .args(["--output", "HEADLESS-1", "--duration-seconds", "2"])
            .spawn()
            .unwrap(),
    );
    let screenshot = directory.path().join("capture.png");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        thread::sleep(Duration::from_millis(50));
        let frame = session.screenshot(&screenshot);
        if frame.dimensions() == (320, 180) && frame.get_pixel(160, 90).0 == [255, 0, 0, 255] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Rust image background was not presented"
        );
    }
    player.wait(Duration::from_secs(4));

    let video = directory.path().join("test.mkv");
    let status = Command::new(&ffmpeg)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x64:r=10",
            "-t",
            "1",
            "-c:v",
            "ffv1",
        ])
        .arg(&video)
        .status()
        .unwrap();
    assert!(status.success());
    let mut player = OwnedChild(
        session
            .command(BINARY)
            .arg("play")
            .arg(&video)
            .arg("--ffmpeg")
            .arg(&ffmpeg)
            .args([
                "--output",
                "HEADLESS-1",
                "--fps",
                "10",
                "--duration-seconds",
                "10",
            ])
            .spawn()
            .unwrap(),
    );
    thread::sleep(Duration::from_millis(600));
    assert!(
        player.0.try_wait().unwrap().is_none(),
        "video player exited early"
    );
    let first = session.screenshot(&screenshot);
    thread::sleep(Duration::from_millis(300));
    let second = session.screenshot(&screenshot);
    assert_eq!(second.dimensions(), (320, 180));
    assert_ne!(
        first.as_raw(),
        second.as_raw(),
        "video pixels did not change"
    );

    for command in [
        "output HEADLESS-1 mode 480x270",
        "output HEADLESS-1 scale 2",
    ] {
        let changed = session.command(&swaymsg).arg(command).output().unwrap();
        assert!(
            changed.status.success(),
            "{}",
            String::from_utf8_lossy(&changed.stderr)
        );
        let response: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
        assert_eq!(response[0]["success"], true);
        thread::sleep(Duration::from_millis(300));
        assert!(
            player.0.try_wait().unwrap().is_none(),
            "resize/scale stopped playback"
        );
    }
    let resized = session.screenshot(&screenshot);
    assert_eq!(resized.dimensions(), (480, 270));
    assert!(resized.pixels().any(|pixel| pixel.0[..3] != [0, 0, 0]));
    let pid = Pid::from_raw(player.0.id() as i32).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    player.wait(Duration::from_secs(3));
    let cleared = session.screenshot(&screenshot);
    assert!(
        cleared.pixels().all(|pixel| pixel.0[..3] == [0, 0, 0]),
        "wallpaper surface was not removed after stop"
    );
    if let Some(scene) = std::env::var_os("WALLPAPER_TEST_SCENE_ROOT") {
        let changed = session
            .command(&swaymsg)
            .arg("output HEADLESS-1 scale 1")
            .output()
            .unwrap();
        assert!(
            changed.status.success(),
            "{}",
            String::from_utf8_lossy(&changed.stderr)
        );
        thread::sleep(Duration::from_millis(300));
        let mut scene_player = OwnedChild(
            session
                .command(BINARY)
                .arg("play")
                .arg(&scene)
                .args([
                    "--output",
                    "HEADLESS-1",
                    "--fps",
                    "1",
                    "--duration-seconds",
                    "10",
                ])
                .spawn()
                .unwrap(),
        );
        thread::sleep(Duration::from_millis(800));
        assert!(
            scene_player.0.try_wait().unwrap().is_none(),
            "scene player exited early"
        );
        let first = session.screenshot(&screenshot);
        thread::sleep(Duration::from_millis(3000));
        let second = session.screenshot(&screenshot);
        assert_eq!(second.dimensions(), (480, 270));
        assert_ne!(
            first.as_raw(),
            second.as_raw(),
            "scene pixels did not change"
        );
        assert!(
            second.pixels().any(|pixel| pixel.0[..3] != [0, 0, 0]),
            "scene did not present visible pixels"
        );
        scene_player.wait(Duration::from_secs(16));
        let cleared = session.screenshot(&screenshot);
        assert!(
            cleared.pixels().all(|pixel| pixel.0[..3] == [0, 0, 0]),
            "scene surface was not removed after stop"
        );
    }
    eprintln!("verified: outputs, image pixels, changing video, resize, scale, duration, SIGTERM and unmap in isolated Sway");
}
