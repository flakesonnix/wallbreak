//! Real CLI diagnostics and controlled subprocess lifecycle tests. No desktop use.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use wallbreak::gui::Player;

const CLI: &str = env!("CARGO_BIN_EXE_wallbreak");

fn helper_args(mode: &str, marker: &Path) -> Vec<String> {
    // Libtest accepts repeated --skip filters. These unused filters carry
    // fixture parameters without global environment changes or a shell.
    [
        "--exact",
        "player_helper",
        "--ignored",
        "--nocapture",
        "--skip",
        mode,
        "--skip",
        marker.to_str().unwrap(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn start_helper(player: &mut Player, mode: &str, marker: &Path) {
    player
        .start(
            &std::env::current_exe().unwrap(),
            &helper_args(mode, marker),
            mode.into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(marker.with_extension("ready"))
        .is_ok_and(|pid| pid.parse::<u32>().is_ok())
    {
        assert!(
            player.is_running().unwrap(),
            "helper exited: {}",
            player.diagnostics()
        );
        assert!(Instant::now() < deadline, "helper never became ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(player: &mut Player) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while player.is_running().unwrap() {
        assert!(Instant::now() < deadline, "player did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!player.has_pending_work());
}

#[test]
fn captures_real_cli_rejection_without_connecting_to_a_desktop() {
    let mut player = Player::new();
    player
        .start(
            &PathBuf::from(CLI),
            &vec!["play", "unused.png", "--fps", "0"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "invalid invocation".into(),
        )
        .unwrap();
    wait_for_exit(&mut player);
    assert_eq!(player.exit_status().unwrap().code(), Some(2));
    let diagnostics = player.diagnostics();
    assert!(diagnostics.contains("error:"), "{diagnostics}");
    assert!(diagnostics.contains("--fps"), "{diagnostics}");
    player.stop().unwrap();
    assert_eq!(player.diagnostics(), diagnostics);
    assert_eq!(player.exit_status().unwrap().code(), Some(2));
}

#[test]
fn captures_nonzero_exit_and_retains_final_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let mut player = Player::new();
    player
        .start(
            &std::env::current_exe().unwrap(),
            &helper_args("exit", &directory.path().join("unused")),
            "failure".into(),
        )
        .unwrap();
    wait_for_exit(&mut player);
    assert_eq!(player.exit_status().unwrap().code(), Some(37));
    assert!(player.diagnostics().contains("controlled playback failure"));
    assert_eq!(player.description(), "failure");
}

#[test]
fn stop_sends_term_and_waits_for_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("restoration");
    let mut player = Player::new();
    assert!(!player.is_running().unwrap());
    start_helper(&mut player, "term", &marker);
    player.stop().unwrap();
    assert_eq!(fs::read_to_string(marker).unwrap(), "restored");
    assert!(player.exit_status().unwrap().success());
    assert!(player.diagnostics().contains("RESTORED"));
    assert!(!player.diagnostics().contains("SIGKILL"));
    assert!(!player.has_pending_work());
    assert!(player.description().is_empty());
    player.stop().unwrap();
}

#[test]
fn replacement_and_drop_use_the_same_graceful_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let mut player = Player::new();
    start_helper(&mut player, "term", &first);
    start_helper(&mut player, "term", &second);
    assert_eq!(fs::read_to_string(first).unwrap(), "restored");
    assert!(!second.exists());
    drop(player);
    assert_eq!(fs::read_to_string(second).unwrap(), "restored");
}

#[test]
fn unresponsive_child_is_escalated_and_reaped_with_a_warning() {
    use std::os::unix::process::ExitStatusExt;

    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("unresponsive");
    let mut player = Player::new();
    start_helper(&mut player, "ignore-term", &marker);
    let start = Instant::now();
    player.stop().unwrap();
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(player.exit_status().unwrap().signal(), Some(9));
    assert!(player
        .diagnostics()
        .contains("restoration may not have completed"));
    assert!(!marker.exists());
    assert!(!player.has_pending_work());
}

#[test]
fn stderr_is_continuously_drained_and_bounded_even_without_polling() {
    let directory = tempfile::tempdir().unwrap();
    let mut player = Player::new();
    start_helper(&mut player, "flood", &directory.path().join("restoration"));
    // Do not poll the child while it writes much more than a pipe can hold.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !player.diagnostics().contains("FLOOD_FINISHED") {
        assert!(Instant::now() < deadline, "stderr was not drained");
        thread::sleep(Duration::from_millis(10));
    }
    let diagnostics = player.diagnostics();
    assert!(diagnostics.contains("stderr truncated"));
    assert!(diagnostics.len() < 33 * 1024);
    player.stop().unwrap();
    assert!(player.diagnostics().contains("RESTORED"));
    assert!(player.diagnostics().len() < 33 * 1024);
}

#[cfg(target_os = "linux")]
#[test]
fn an_open_stderr_writer_cannot_hold_shutdown_open() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("restoration");
    let mut player = Player::new();
    start_helper(&mut player, "term", &marker);
    let pid = fs::read_to_string(marker.with_extension("ready")).unwrap();
    // Retain a writer ourselves, like an inherited stderr FD, without spawning
    // descendants or relinquishing ownership of any helper process.
    let writer = fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{pid}/fd/2"))
        .unwrap();
    let start = Instant::now();
    player.stop().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(player.diagnostics().contains("capture stopped before EOF"));
    assert!(player.diagnostics().contains("RESTORED"));
    assert!(player.exit_status().unwrap().success());
    drop(writer);
}

#[test]
fn spawn_failure_is_reported_without_pending_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let mut player = Player::new();
    let error = player
        .start(
            &directory.path().join("missing-binary"),
            &[],
            "missing".into(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("cannot start"));
    assert!(!player.has_pending_work());
    assert!(player.exit_status().is_none());
}

#[cfg(feature = "gui")]
#[test]
fn idle_panel_repaints_and_renders_exit_status_and_cli_diagnostics() {
    use eframe::egui;
    use wallbreak::gui::panel::Panel;

    let directory = tempfile::tempdir().unwrap();
    let mut panel = Panel::new();
    let ctx = egui::Context::default();
    // Settle egui's initial repaint requests before checking our polling timer.
    for _ in 0..5 {
        let _ = ctx.run(Default::default(), |ctx| panel.show(ctx));
    }
    let idle = ctx.run(Default::default(), |ctx| panel.show(ctx));
    assert!(
        idle.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::from_millis(100)
    );
    start_helper(
        &mut panel.player,
        "term",
        &directory.path().join("restoration"),
    );
    for _ in 0..5 {
        let _ = ctx.run(Default::default(), |ctx| panel.show(ctx));
    }
    let output = ctx.run(Default::default(), |ctx| panel.show(ctx));
    assert!(
        output.viewport_output[&egui::ViewportId::ROOT].repaint_delay <= Duration::from_millis(100)
    );
    panel
        .player
        .start(
            &PathBuf::from(CLI),
            &[
                "play".into(),
                "unused.png".into(),
                "--fps".into(),
                "0".into(),
            ],
            "invalid CLI".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = ctx.run(Default::default(), |ctx| panel.show(ctx));
        if panel.player.exit_status().is_some() {
            let text = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Player exited:"), "{text}");
            assert!(text.contains("error:"), "{text}");
            assert!(text.contains("--fps"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "panel missed player exit");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "controlled subprocess fixture, executed by the player tests"]
fn player_helper() {
    let args: Vec<_> = std::env::args().collect();
    let parameters: Vec<_> = args
        .windows(2)
        .filter(|pair| pair[0] == "--skip")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(parameters.len(), 2, "helper requires explicit parameters");
    let (mode, marker) = (parameters[0], Path::new(parameters[1]));
    if mode == "exit" {
        eprintln!("controlled playback failure");
        std::process::exit(37);
    }
    assert!(["term", "ignore-term", "flood"].contains(&mode));
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone()).unwrap();
    fs::write(
        marker.with_extension("ready"),
        std::process::id().to_string(),
    )
    .unwrap();
    eprintln!("HELPER_READY");
    if mode == "flood" {
        let mut stderr = io::stderr().lock();
        for _ in 0..512 {
            stderr.write_all(&[b'x'; 4096]).unwrap();
        }
        writeln!(stderr, "FLOOD_FINISHED").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if mode != "ignore-term" && stop.load(Ordering::Relaxed) {
            fs::write(marker, "restored").unwrap();
            eprintln!("RESTORED");
            return;
        }
        assert!(Instant::now() < deadline, "helper was not cleaned up");
        thread::sleep(Duration::from_millis(10));
    }
}
