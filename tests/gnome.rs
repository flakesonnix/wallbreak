//! Real gsettings round trips with the private keyfile backend, never dconf or
//! the running desktop. Explicitly requested tests require the actual GLib CLI.

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use image::{Rgba, RgbaImage};
use rustix::process::{Pid, Signal, kill_process};

const BINARY: &str = env!("CARGO_BIN_EXE_wallbreak");
const SCHEMA: &str = "org.gnome.desktop.background";
const KEYS: [&str; 3] = ["picture-uri", "picture-uri-dark", "picture-options"];

struct Session {
    directory: tempfile::TempDir,
    settings: PathBuf,
    image: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl Session {
    fn new() -> Self {
        let settings = PathBuf::from(
            std::env::var_os("WALLBREAK_TEST_GSETTINGS")
                .expect("set WALLBREAK_TEST_GSETTINGS to an actual gsettings binary"),
        );
        assert!(settings.is_absolute() && settings.is_file());
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
        let path = std::env::join_paths(
            std::iter::once(settings.parent().unwrap().to_owned()).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
        )
        .unwrap();
        let environment = vec![
            ("GSETTINGS_BACKEND".into(), "keyfile".into()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/nonexistent-wallbreak-test-bus".into(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                directory.path().join("config").into(),
            ),
            ("XDG_RUNTIME_DIR".into(), runtime.into()),
            ("HOME".into(), directory.path().into()),
            ("XDG_CURRENT_DESKTOP".into(), "GNOME".into()),
            ("PATH".into(), path),
        ];
        let image = directory.path().join("image space #?\u{e4}'quote.png");
        RgbaImage::from_pixel(16, 16, Rgba([7, 23, 59, 255]))
            .save(&image)
            .unwrap();
        let session = Self {
            directory,
            settings,
            image,
            environment,
        };
        for (key, value) in KEYS.into_iter().zip([
            "file:///previous-light.png",
            "file:///previous-dark.png",
            "wallpaper",
        ]) {
            session.set(key, value);
        }
        session
    }

    fn command(&self, executable: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .envs(self.environment.iter().map(|(key, value)| (key, value)))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET");
        command
    }

    fn get(&self, key: &str) -> String {
        let output = self
            .command(&self.settings)
            .args(["get", SCHEMA, key])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn set(&self, key: &str, value: &str) {
        let output = self
            .command(&self.settings)
            .args(["set", SCHEMA, key, &serde_json::to_string(value).unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn snapshot(&self) -> [String; 3] {
        KEYS.map(|key| self.get(key))
    }

    fn player(&self, duration: &str) -> Command {
        let mut command = self.command(BINARY);
        command
            .arg("play")
            .arg(&self.image)
            .args(["--duration-seconds", duration])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    fn active_uri(&self) -> String {
        // Ask GLib itself to format the desired URI; no duplicate GVariant parser.
        let previous = self.get("picture-uri");
        let uri = url::Url::from_file_path(&self.image).unwrap();
        self.set("picture-uri", uri.as_str());
        let formatted = self.get("picture-uri");
        let output = self
            .command(&self.settings)
            .args(["set", SCHEMA, "picture-uri", &previous])
            .output()
            .unwrap();
        assert!(output.status.success());
        formatted
    }

    fn await_active(&self, player: &mut OwnedChild, uri: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.get("picture-uri") == uri
                && self.get("picture-uri-dark") == uri
                && self.get("picture-options") != "'wallpaper'"
            {
                break;
            }
            assert!(
                player.0.try_wait().unwrap().is_none(),
                "GNOME player exited before applying the image"
            );
            assert!(
                Instant::now() < deadline,
                "GNOME settings did not become active"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

struct OwnedChild(Child);

impl OwnedChild {
    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "GNOME player failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "GNOME player did not finish");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate(&mut self) {
        kill_process(Pid::from_raw(self.0.id() as i32).unwrap(), Signal::TERM).unwrap();
        self.finish();
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

#[test]
#[ignore = "requires WALLBREAK_TEST_GSETTINGS; uses only an isolated keyfile backend"]
fn gnome_image_applies_both_themes_and_restores_after_duration() {
    let session = Session::new();
    let previous = session.snapshot();
    let uri = session.active_uri();
    let mut player = OwnedChild(session.player("2").spawn().unwrap());
    session.await_active(&mut player, &uri);
    assert_eq!(session.get("picture-options"), "'zoom'");
    player.finish();
    assert_eq!(session.snapshot(), previous);
}

#[test]
#[ignore = "requires WALLBREAK_TEST_GSETTINGS; uses only an isolated keyfile backend"]
fn gnome_term_restores_only_settings_it_still_owns() {
    let session = Session::new();
    let previous = session.snapshot();
    let uri = session.active_uri();
    let mut player = OwnedChild(
        session
            .player("20")
            .args(["--fit", "contain"])
            .spawn()
            .unwrap(),
    );
    session.await_active(&mut player, &uri);
    assert_eq!(session.get("picture-options"), "'scaled'");
    session.set("picture-uri", "file:///users-new-choice.png");
    let changed = session.get("picture-uri");
    player.terminate();
    assert_eq!(
        session.snapshot(),
        [changed, previous[1].clone(), previous[2].clone()]
    );
}

#[test]
#[ignore = "requires WALLBREAK_TEST_GSETTINGS; uses only an isolated keyfile backend"]
fn gnome_rejects_animation_output_selection_and_corrupt_images_without_writes() {
    let session = Session::new();
    let previous = session.snapshot();
    let video = session.directory.path().join("test.mp4");
    fs::write(&video, b"unused video; no decoder should start").unwrap();
    let rejected = session
        .command(BINARY)
        .arg("play")
        .arg(video)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("GNOME animation requires"));
    let rejected = session
        .player("1")
        .args(["--output", "eDP-1"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("omit --output"));
    fs::write(&session.image, b"not a PNG").unwrap();
    let rejected = session.player("1").output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("image decoding failed"));
    assert_eq!(session.snapshot(), previous);
}

#[test]
#[ignore = "requires WALLBREAK_TEST_GSETTINGS; uses only an isolated keyfile backend"]
fn gnome_rejects_overlapping_players_without_losing_original_settings() {
    let session = Session::new();
    let previous = session.snapshot();
    let uri = session.active_uri();
    let mut first = OwnedChild(session.player("20").spawn().unwrap());
    session.await_active(&mut first, &uri);
    let rejected = session.player("1").output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("already active"));
    assert_eq!(session.get("picture-uri"), uri);
    first.terminate();
    assert_eq!(session.snapshot(), previous);
}

#[test]
#[ignore = "requires WALLBREAK_TEST_GSETTINGS; uses only an isolated keyfile backend"]
fn gnome_rolls_back_partial_writes_even_when_helper_fails_after_mutation() {
    let session = Session::new();
    let previous = session.snapshot();
    let directory = session.directory.path().join("shim");
    fs::create_dir(&directory).unwrap();
    let wrapper = directory.join("gsettings");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("sh"))
        .find(|path| path.is_file())
        .expect("the failure-injection test requires sh on PATH");
    let source = include_str!("fixtures/failing-gsettings.sh").replacen(
        "#!/bin/sh",
        &format!("#!{}", shell.display()),
        1,
    );
    fs::write(&wrapper, source).unwrap();
    fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(std::iter::once(directory).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let rejected = session
        .player("1")
        .env("PATH", path)
        .env("WALLBREAK_TEST_REAL_GSETTINGS", &session.settings)
        .env(
            "WALLBREAK_TEST_FAILURE_MARKER",
            session.directory.path().join("failed-once"),
        )
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("synthetic post-write failure"));
    assert_eq!(session.snapshot(), previous);
}
