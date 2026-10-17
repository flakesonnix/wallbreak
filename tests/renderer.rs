#![cfg(feature = "gui")]
//! Renderer CLI and pre-window lifecycle checks, without a compositor or desktop.

use std::{
    fs,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{kill_process, Pid, Signal};

const BINARY: &str = env!("CARGO_BIN_EXE_wallbreak-renderer");

fn arguments() -> Vec<&'static str> {
    vec![
        "--source",
        "/not-read-before-validation/project.json",
        "--session-token",
        "01234567-89ab-cdef-0123-456789abcdef",
        "--width",
        "1920",
        "--height",
        "1080",
        "--logical-width",
        "960",
        "--logical-height",
        "540",
        "--fps",
        "30",
        "--fit",
        "cover",
        "--ffmpeg",
        "must-not-execute-ffmpeg",
    ]
}

fn invoke(arguments: &[&str]) -> Output {
    Command::new(BINARY)
        .args(arguments)
        .env_remove("WAYLAND_SOCKET")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .output()
        .unwrap()
}

#[test]
fn help_works_without_a_bridge_and_has_no_preview_mode() {
    let output = invoke(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in [
        "--source",
        "--session-token",
        "--width",
        "--height",
        "--logical-width",
        "--logical-height",
        "--fps",
        "--fit",
        "--ffmpeg",
    ] {
        assert!(help.contains(flag), "{help}");
    }
    assert!(!help.contains("--preview"));
}

#[test]
fn standalone_invocation_is_rejected_before_source_or_decoder_access() {
    let output = invoke(&arguments());
    assert!(!output.status.success());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("WAYLAND_SOCKET is required"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("cannot resolve input"), "{diagnostic}");
}

#[test]
fn invalid_inherited_socket_is_rejected_without_falling_back_to_the_desktop() {
    for value in ["", "not-a-fd", "-1", "0", "2", "2147483647"] {
        let output = Command::new(BINARY)
            .args(arguments())
            .env("WAYLAND_SOCKET", value)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.contains("WAYLAND_SOCKET"), "{diagnostic}");
        assert!(!diagnostic.contains("regular media file"), "{diagnostic}");
    }
}

#[test]
fn all_bridge_arguments_are_required() {
    let args = arguments();
    for index in (0..args.len()).step_by(2) {
        let mut missing = args.clone();
        missing.drain(index..index + 2);
        let output = invoke(&missing);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(args[index]));
    }
}

#[test]
fn malformed_tokens_dimensions_fps_and_fit_are_cli_errors() {
    for (flag, value) in [
        ("--session-token", "01234567-89AB-cdef-0123-456789abcdef"),
        ("--session-token", "0123456789abcdef0123456789abcdef"),
        ("--width", "0"),
        ("--height", "0"),
        ("--width", "4294967296"),
        ("--logical-width", "0"),
        ("--logical-height", "16385"),
        ("--fps", "0"),
        ("--fps", "61"),
        ("--fit", "stretch"),
    ] {
        let mut args = arguments();
        let index = args.iter().position(|argument| *argument == flag).unwrap();
        args[index + 1] = value;
        let output = invoke(&args);
        assert_eq!(output.status.code(), Some(2), "{flag} {value}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(flag));
    }
}

#[test]
fn oversized_frames_and_relative_sources_fail_before_wayland_or_source_access() {
    for (flag, value, message) in [
        ("--width", "4294967295", "64 MiB"),
        ("--source", "project.json", "absolute file path"),
    ] {
        let mut args = arguments();
        let index = args.iter().position(|argument| *argument == flag).unwrap();
        args[index + 1] = value;
        let output = invoke(&args);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.contains(message), "{diagnostic}");
        assert!(!diagnostic.contains("WAYLAND_SOCKET"), "{diagnostic}");
    }
}

struct PendingRenderer(Child);

impl PendingRenderer {
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "renderer did not finish cleanup");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for PendingRenderer {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = kill_process(Pid::from_child(&self.0), Signal::TERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
        }
        let _ = self.0.wait();
    }
}

fn decoder_renderer(
    directory: &Path,
    body: &str,
) -> (PendingRenderer, UnixStream, PathBuf, PathBuf) {
    let source = directory.join("clip.mp4");
    fs::write(&source, b"controlled decoder fixture, not a real video").unwrap();
    let decoder = directory.join("decoder");
    fs::write(&decoder, format!("#!/bin/sh\nwhile [ \"$1\" != -i ]; do shift; done\nshift\nif [ -S \"/proc/self/fd/$WAYLAND_SOCKET\" ]; then exit 9; fi\nprintf '%s' \"$$\" > \"$1.pid\"\n{body}\n")).unwrap();
    fs::set_permissions(&decoder, fs::Permissions::from_mode(0o700)).unwrap();
    let log = directory.join("stderr");
    let (socket, bridge) = UnixStream::pair().unwrap();
    let mut args = arguments();
    args[1] = source.to_str().unwrap();
    for flag in ["--width", "--height", "--logical-width", "--logical-height"] {
        let index = args.iter().position(|argument| *argument == flag).unwrap();
        args[index + 1] = "1";
    }
    *args.last_mut().unwrap() = decoder.to_str().unwrap();
    let mut command = Command::new(BINARY);
    command
        .args(args)
        .env("WAYLAND_SOCKET", socket.as_raw_fd().to_string())
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap());
    // Clear CLOEXEC only in the child; other parallel tests cannot inherit this FD.
    unsafe {
        command.pre_exec(move || {
            rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::empty())
                .map_err(std::io::Error::from)
        });
    }
    let child = command.spawn().unwrap();
    drop(command);
    let mut renderer = PendingRenderer(child);
    let pid_file = source.with_extension("mp4.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&pid_file).is_ok_and(|pid| pid.parse::<u32>().is_ok()) {
        assert!(
            renderer.0.try_wait().unwrap().is_none(),
            "renderer exited before decoder startup: {}",
            fs::read_to_string(&log).unwrap()
        );
        assert!(Instant::now() < deadline, "decoder fixture did not start");
        thread::sleep(Duration::from_millis(10));
    }
    (renderer, bridge, pid_file, log)
}

fn assert_pre_window_cleanup(mut bridge: UnixStream, pid_file: &Path) {
    let pid: u32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "owned decoder was not reaped"
    );
    // A complete first frame was never available, so no Wayland window traffic is allowed.
    bridge
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(bridge.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn sigint_and_sigterm_cancel_first_frame_wait_and_reap_the_decoder() {
    for signal in [Signal::INT, Signal::TERM] {
        let directory = tempfile::tempdir().unwrap();
        // exec keeps the stalled decoder a single owned process, without descendants.
        let (mut renderer, bridge, pid_file, log) =
            decoder_renderer(directory.path(), "exec sleep 60");
        kill_process(Pid::from_child(&renderer.0), signal).unwrap();
        let status = renderer.wait(Duration::from_secs(3));
        assert!(status.success(), "{}", fs::read_to_string(log).unwrap());
        assert_pre_window_cleanup(bridge, &pid_file);
    }
}

#[test]
fn first_frame_timeout_is_nonzero_and_reaps_the_decoder_without_mapping_a_window() {
    let directory = tempfile::tempdir().unwrap();
    let (mut renderer, bridge, pid_file, log) = decoder_renderer(directory.path(), "exec sleep 60");
    let status = renderer.wait(Duration::from_secs(20));
    assert!(!status.success());
    let diagnostic = fs::read_to_string(log).unwrap();
    assert!(
        diagnostic.contains("no complete first frame within 15s"),
        "{diagnostic}"
    );
    assert_pre_window_cleanup(bridge, &pid_file);
}

#[test]
fn partial_first_frame_reports_the_decoder_failure_without_mapping_a_window() {
    let directory = tempfile::tempdir().unwrap();
    let (mut renderer, bridge, pid_file, log) =
        decoder_renderer(directory.path(), "printf '\\003'; exit 7");
    assert!(!renderer.wait(Duration::from_secs(5)).success());
    let diagnostic = fs::read_to_string(log).unwrap();
    assert!(
        diagnostic.contains("FFmpeg did not produce a complete frame"),
        "{diagnostic}"
    );
    assert_pre_window_cleanup(bridge, &pid_file);
}

#[test]
fn native_setup_failure_after_a_complete_frame_still_reaps_the_decoder() {
    let directory = tempfile::tempdir().unwrap();
    let (mut renderer, bridge, pid_file, log) =
        decoder_renderer(directory.path(), "printf '\\076\\146\\357'; exec sleep 60");
    // Close our private peer, not a real compositor. Native setup must fail closed.
    drop(bridge);
    assert!(!renderer.wait(Duration::from_secs(5)).success());
    let diagnostic = fs::read_to_string(log).unwrap();
    assert!(
        diagnostic.contains("renderer window/Wayland connection failed"),
        "{diagnostic}"
    );
    let pid: u32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "owned decoder was not reaped"
    );
}
