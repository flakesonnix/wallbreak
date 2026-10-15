#![cfg(feature = "gui")]
//! Headless smoke test for the wallbreak control panel.
//!
//! Requires explicit binaries and creates only an isolated Sway compositor.
//! The panel must map a visible, non-black window without user interaction.

use std::{
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BINARY: &str = env!("CARGO_BIN_EXE_wallbreak-gui");

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
            thread::sleep(Duration::from_millis(50));
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

fn non_black_pixels(image: &image::RgbaImage) -> usize {
    image
        .pixels()
        .filter(|pixel| pixel.0[..3] != [0, 0, 0])
        .count()
}

#[test]
#[ignore = "requires explicit WALLPAPER_TEST_SWAY, SWAYMSG, GRIM binaries and GUI runtime libraries; creates only an isolated compositor"]
fn panel_maps_a_visible_window_in_isolated_sway() {
    for name in [
        "WALLPAPER_TEST_SWAY",
        "WALLPAPER_TEST_SWAYMSG",
        "WALLPAPER_TEST_GRIM",
    ] {
        if std::env::var_os(name).is_none() {
            eprintln!("skipping GUI check without {name}");
            return;
        }
    }
    let binary = |name: &str| {
        std::env::var_os(name).unwrap_or_else(|| panic!("set {name} to an existing binary"))
    };
    let sway = binary("WALLPAPER_TEST_SWAY");
    let grim = binary("WALLPAPER_TEST_GRIM");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    let config = directory.path().join("sway.conf");
    fs::write(&config, "output HEADLESS-1 mode 800x600\n").unwrap();
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
    let (display, _ipc) = loop {
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
    let screenshot = directory.path().join("panel.png");
    let mut command = Command::new(BINARY);
    command
        .arg("--smoke-test")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("WAYLAND_DISPLAY", &display)
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DISPLAY")
        .env_remove("SWAYSOCK")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Systems without wayland-client/libGL in the loader path (for example a
    // plain NixOS user session) can point the panel at them explicitly.
    if let Some(library_path) = std::env::var_os("WALLPAPER_TEST_LD_LIBRARY_PATH") {
        command.env("LD_LIBRARY_PATH", library_path);
    }
    let mut panel = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let visible = loop {
        thread::sleep(Duration::from_millis(500));
        let result = Command::new(&grim)
            .arg("-o")
            .arg("HEADLESS-1")
            .arg(&screenshot)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("WAYLAND_DISPLAY", &display)
            .env_remove("DISPLAY")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let frame = image::open(&screenshot).unwrap().into_rgba8();
        assert_eq!(frame.dimensions(), (800, 600));
        if non_black_pixels(&frame) > 1000 {
            break true;
        }
        assert!(
            Instant::now() < deadline,
            "panel window did not become visible"
        );
        assert!(
            panel.0.try_wait().unwrap().is_none(),
            "panel exited before becoming visible"
        );
    };
    assert!(visible);
    panel.wait(Duration::from_secs(15));
}
