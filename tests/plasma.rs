#![cfg(target_os = "linux")]

// Exercise the private connection boundary without exposing a production test API.
use wallbreak::{media, project};
include!("../src/plasma.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        io::{BufRead, BufReader},
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Child, Command, Stdio},
        sync::{Mutex, mpsc},
        thread,
    };
    use zbus::{
        MatchRule,
        blocking::{MessageIterator, fdo::MonitoringProxy},
        message::Type,
    };

    const URI: &str = "file:///private/new%20image%23%3F%22.png";
    const ACTIONS: [&str; 5] = ["snapshot", "switch", "apply", "verify", "restore"];

    struct PrivateBus {
        child: Child,
        address: String,
        _directory: tempfile::TempDir,
    }

    impl PrivateBus {
        fn new() -> Self {
            let program = std::env::var_os("WALLBREAK_TEST_DBUS_DAEMON")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                        .map(|directory| directory.join("dbus-daemon"))
                        .find(|path| path.is_file())
                })
                .expect("protocol tests require dbus-daemon; set WALLBREAK_TEST_DBUS_DAEMON to its absolute path");
            assert!(!program.as_os_str().is_empty(), "empty dbus-daemon path");
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let mut daemon = Command::new(program);
            if let Some(config) = std::env::var_os("WALLBREAK_TEST_DBUS_CONFIG") {
                daemon.arg(format!("--config-file={}", config.to_string_lossy()));
            } else {
                daemon.arg("--session");
            }
            let child = daemon
                .args(["--nofork", "--nopidfile", "--print-address=1"])
                .arg(format!(
                    "--address=unix:path={}",
                    directory.path().join("bus").display()
                ))
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("private dbus-daemon must start");
            let mut bus = Self {
                child,
                address: String::new(),
                _directory: directory,
            };
            let stdout = bus.child.stdout.take().unwrap();
            let (sender, receiver) = mpsc::channel();
            let reader = thread::spawn(move || {
                let mut address = String::new();
                let result = BufReader::new(stdout)
                    .read_line(&mut address)
                    .map(|_| address);
                let _ = sender.send(result);
            });
            let address = receiver.recv_timeout(Duration::from_secs(5));
            if address.is_err() {
                bus.terminate();
            }
            reader.join().unwrap();
            bus.address = address
                .expect("private bus did not publish an address")
                .unwrap()
                .trim()
                .into();
            assert!(bus.address.starts_with("unix:"));
            assert!(bus.child.try_wait().unwrap().is_none());
            bus
        }

        fn connection(&self) -> Connection {
            connection::Builder::address(self.address.as_str())
                .unwrap()
                .method_timeout(CALL_TIMEOUT)
                .build()
                .unwrap()
        }

        fn terminate(&mut self) {
            if self.child.try_wait().unwrap().is_none() {
                self.child.kill().unwrap();
            }
            self.child.wait().unwrap();
            assert!(self.child.try_wait().unwrap().is_some());
        }

        fn observed_calls(
            &self,
            connection: &Connection,
            stop: Arc<AtomicBool>,
        ) -> (Result<()>, Vec<String>) {
            let monitor = self.connection();
            let mut messages = MessageIterator::from(&monitor);
            let rule = MatchRule::builder()
                .msg_type(Type::MethodCall)
                .sender(connection.unique_name().unwrap().as_str())
                .unwrap()
                .build();
            MonitoringProxy::new(&monitor)
                .unwrap()
                .become_monitor(&[rule], 0)
                .unwrap();
            let (sender, receiver) = mpsc::channel();
            let reader = thread::spawn(move || {
                let mut calls = Vec::new();
                for message in &mut messages {
                    let message = message.unwrap();
                    if message.message_type() != Type::MethodCall {
                        continue;
                    }
                    let header = message.header();
                    let member = header.member().unwrap().as_str();
                    // GetId is an ordered barrier on the same sender, not a sleep.
                    if member == "GetId" {
                        break;
                    }
                    calls.push(member.to_owned());
                }
                let _ = sender.send(calls);
            });
            let result = run_connected(connection.clone(), URI, &options(), stop);
            let barrier = DBusProxy::new(connection).unwrap().get_id();
            let calls = receiver.recv_timeout(Duration::from_secs(5));
            monitor.close().unwrap();
            reader.join().unwrap();
            barrier.unwrap();
            (
                result,
                calls.expect("private bus monitor did not reach the barrier"),
            )
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            self.terminate();
        }
    }

    fn snapshot() -> serde_json::Value {
        json!([
            {"id": 7, "plugin": "org.kde.slideshow", "settings": {
                "Image": "file:///old image #?\"quote\\path.png", "FillMode": 6,
                "Color": "#123456", "Blur": true
            }},
            {"id": 42, "plugin": "org.kde.image", "settings": {
                "Image": "file:///other.png", "FillMode": 0,
                "Color": "#abcdef", "Blur": false
            }}
        ])
    }

    #[derive(Clone)]
    enum Reply {
        Text(String),
        Error,
    }

    struct Call {
        request: serde_json::Value,
        received: Instant,
        replied: Option<Instant>,
    }

    struct StubState {
        snapshot: String,
        calls: Vec<Call>,
        replies: BTreeMap<&'static str, Reply>,
        delays: BTreeMap<&'static str, Duration>,
        cancel_after: Option<(&'static str, Arc<AtomicBool>)>,
    }

    impl Default for StubState {
        fn default() -> Self {
            Self {
                snapshot: snapshot().to_string(),
                calls: Vec::new(),
                replies: BTreeMap::new(),
                delays: BTreeMap::new(),
                cancel_after: None,
            }
        }
    }

    struct Stub(Arc<Mutex<StubState>>);

    #[zbus::interface(name = "org.kde.PlasmaShell")]
    impl Stub {
        #[zbus(name = "evaluateScript")]
        fn evaluate_script(
            &self,
            script: &str,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<String> {
            assert_eq!(header.path().unwrap().as_str(), "/PlasmaShell");
            assert_eq!(header.member().unwrap().as_str(), "evaluateScript");
            assert!(header.destination().unwrap().as_str().starts_with(':'));
            let literal = script
                .lines()
                .next()
                .unwrap()
                .strip_prefix("var request = JSON.parse(")
                .unwrap()
                .strip_suffix(");")
                .unwrap();
            let encoded: String = serde_json::from_str(literal).unwrap();
            let request: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            let action = request["action"].as_str().unwrap().to_owned();
            assert!(
                ACTIONS.contains(&action.as_str()),
                "unknown action {action}"
            );
            let mut state = self.0.lock().unwrap();
            let reply = state
                .replies
                .get(action.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    Reply::Text(if action == "snapshot" {
                        state.snapshot.clone()
                    } else {
                        "null".into()
                    })
                });
            let delay = state
                .delays
                .get(action.as_str())
                .copied()
                .unwrap_or_default();
            let index = state.calls.len();
            state.calls.push(Call {
                request,
                received: Instant::now(),
                replied: None,
            });
            drop(state);
            thread::sleep(delay);
            let mut state = self.0.lock().unwrap();
            state.calls[index].replied = Some(Instant::now());
            if let Some((after, stop)) = &state.cancel_after
                && *after == action
            {
                stop.store(true, Ordering::Relaxed);
            }
            match reply {
                Reply::Text(text) => Ok(text),
                Reply::Error => Err(zbus::fdo::Error::Failed(format!("stub {action} rejected"))),
            }
        }
    }

    struct Fixture {
        // Connections drop before reaping the owned daemon.
        service: Connection,
        state: Arc<Mutex<StubState>>,
        bus: PrivateBus,
    }

    impl Fixture {
        fn new(state: StubState) -> Self {
            let bus = PrivateBus::new();
            let state = Arc::new(Mutex::new(state));
            let service = connection::Builder::address(bus.address.as_str())
                .unwrap()
                .method_timeout(CALL_TIMEOUT)
                .serve_at("/PlasmaShell", Stub(state.clone()))
                .unwrap()
                .name("org.kde.plasmashell")
                .unwrap()
                .build()
                .unwrap();
            Self {
                service,
                state,
                bus,
            }
        }

        fn run(&self, options: &PlaybackOptions, stop: Arc<AtomicBool>) -> Result<()> {
            run_connected(self.bus.connection(), URI, options, stop)
        }

        fn assert_actions(&self, expected: &[&str]) {
            let state = self.state.lock().unwrap();
            let actions: Vec<_> = state
                .calls
                .iter()
                .map(|call| call.request["action"].as_str().unwrap())
                .collect();
            assert_eq!(actions, expected);
        }
    }

    fn options() -> PlaybackOptions {
        PlaybackOptions {
            fps: 37,
            fit: Fit::Contain,
            ffmpeg: "must-not-run-ffmpeg".into(),
            duration: Some(Duration::ZERO),
        }
    }

    fn stop() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn successful_playback_orders_calls_and_preserves_all_desktops_for_both_fits() {
        for (fit, fill_mode) in [(Fit::Contain, 1), (Fit::Cover, 2)] {
            let fixture = Fixture::new(StubState::default());
            let connection = fixture.bus.connection();
            let owner = connection.unique_name().unwrap().to_string();
            run_connected(
                connection,
                URI,
                &PlaybackOptions { fit, ..options() },
                stop(),
            )
            .unwrap();
            fixture.assert_actions(&ACTIONS);
            let state = fixture.state.lock().unwrap();
            assert_eq!(state.calls[0].request, json!({"action": "snapshot"}));
            for (call, action) in state.calls[1..].iter().zip(&ACTIONS[1..]) {
                assert_eq!(
                    call.request,
                    json!({
                        "action": action,
                        "owner": owner,
                        "partial": matches!(*action, "switch" | "apply"),
                        "previous": snapshot(),
                        "applied": {"Image": URI, "FillMode": fill_mode, "Color": "#000000", "Blur": false}
                    })
                );
            }
        }
    }

    #[test]
    fn duration_starts_after_verification_not_snapshot_or_apply() {
        let fixture = Fixture::new(StubState {
            delays: ACTIONS[..4]
                .iter()
                .map(|&action| (action, Duration::from_millis(70)))
                .collect(),
            ..StubState::default()
        });
        let duration = Duration::from_millis(120);
        fixture
            .run(
                &PlaybackOptions {
                    duration: Some(duration),
                    ..options()
                },
                stop(),
            )
            .unwrap();
        fixture.assert_actions(&ACTIONS);
        let state = fixture.state.lock().unwrap();
        let verified = state.calls[3].replied.unwrap();
        assert!(verified.duration_since(state.calls[0].received) >= Duration::from_millis(280));
        assert!(state.calls[4].received.duration_since(verified) >= duration);
    }

    #[test]
    fn cancellation_before_calls_and_absent_service_never_request_activation() {
        let fixture = Fixture::new(StubState::default());
        let connection = fixture.bus.connection();
        let (result, calls) = fixture
            .bus
            .observed_calls(&connection, Arc::new(AtomicBool::new(true)));
        result.unwrap();
        assert!(calls.is_empty(), "pre-cancelled playback called {calls:?}");
        fixture.assert_actions(&[]);

        fixture.service.release_name("org.kde.plasmashell").unwrap();
        let (result, calls) = fixture.bus.observed_calls(&connection, stop());
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(HELP), "{error}");
        assert!(error.contains("NameHasNoOwner"), "{error}");
        assert_eq!(calls, ["GetNameOwner"]);
        fixture.assert_actions(&[]);
        assert!(
            !DBusProxy::new(&connection)
                .unwrap()
                .name_has_owner("org.kde.plasmashell".try_into().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn cancellation_after_each_call_skips_further_setup_and_restores_owned_settings() {
        for (action, expected) in [
            ("snapshot", vec!["snapshot"]),
            ("switch", vec!["snapshot", "switch", "restore"]),
            ("apply", vec!["snapshot", "switch", "apply", "restore"]),
            ("verify", ACTIONS.to_vec()),
        ] {
            let stop = stop();
            let fixture = Fixture::new(StubState {
                cancel_after: Some((action, stop.clone())),
                ..StubState::default()
            });
            fixture
                .run(
                    &PlaybackOptions {
                        duration: None,
                        ..options()
                    },
                    stop,
                )
                .unwrap();
            fixture.assert_actions(&expected);
        }
    }

    #[test]
    fn malformed_empty_duplicate_and_invalid_snapshot_entries_never_write() {
        let desktop = snapshot()[0].clone();
        let mut cases = vec![
            ("not JSON".into(), "invalid desktop snapshot"),
            ("null".into(), "invalid desktop snapshot"),
            ("{}".into(), "invalid desktop snapshot"),
            ("[{}]".into(), "invalid desktop snapshot"),
            ("[]".into(), "no active desktop containments"),
            (
                json!([desktop, desktop]).to_string(),
                "invalid or duplicate desktop IDs",
            ),
        ];
        for (field, value, expected) in [
            ("id", json!(0), "invalid or duplicate desktop IDs"),
            ("id", json!(-1), "invalid desktop snapshot"),
            (
                "id",
                json!(u64::from(u32::MAX) + 1),
                "invalid desktop snapshot",
            ),
            ("id", json!("7"), "invalid desktop snapshot"),
            ("plugin", json!(""), "no wallpaper plugin"),
            ("plugin", json!(null), "invalid desktop snapshot"),
            ("plugin", json!(7), "invalid desktop snapshot"),
            (
                "settings",
                json!({"Image": "file:///old.png"}),
                "invalid desktop snapshot",
            ),
        ] {
            let mut invalid = snapshot();
            invalid[1][field] = value;
            cases.push((invalid.to_string(), expected));
        }
        for (snapshot, expected) in cases {
            let fixture = Fixture::new(StubState {
                snapshot,
                ..StubState::default()
            });
            let error = format!("{:#}", fixture.run(&options(), stop()).unwrap_err());
            assert!(error.contains(expected), "{expected}: {error}");
            fixture.assert_actions(&["snapshot"]);
        }
    }

    #[test]
    fn snapshot_transport_failure_never_mutates() {
        let fixture = Fixture::new(StubState {
            replies: BTreeMap::from([("snapshot", Reply::Error)]),
            ..StubState::default()
        });
        let error = format!("{:#}", fixture.run(&options(), stop()).unwrap_err());
        assert!(error.contains(HELP), "{error}");
        assert!(error.contains("stub snapshot rejected"), "{error}");
        fixture.assert_actions(&["snapshot"]);
    }

    #[test]
    fn switch_apply_and_verify_failures_including_invalid_acknowledgements_restore() {
        for index in 1..=3 {
            let action = ACTIONS[index];
            for reply in [
                Reply::Error,
                Reply::Text("undefined".into()),
                Reply::Text("\"null\"".into()),
                Reply::Text(String::new()),
                Reply::Text("null\nunexpected".into()),
            ] {
                let expected = match &reply {
                    Reply::Error => format!("wallpaper {action} failed"),
                    Reply::Text(_) => format!("invalid {action} acknowledgement"),
                };
                let fixture = Fixture::new(StubState {
                    replies: BTreeMap::from([(action, reply)]),
                    ..StubState::default()
                });
                let error = format!("{:#}", fixture.run(&options(), stop()).unwrap_err());
                assert!(error.contains(&expected), "{expected}: {error}");
                let mut actions = ACTIONS[..=index].to_vec();
                actions.push("restore");
                fixture.assert_actions(&actions);
                let state = fixture.state.lock().unwrap();
                let mut restored = state.calls[index].request.clone();
                restored["action"] = json!("restore");
                assert_eq!(restored["partial"], json!(action != "verify"));
                assert_eq!(state.calls.last().unwrap().request, restored);
            }
        }
    }

    #[test]
    fn acknowledgements_allow_surrounding_whitespace() {
        let fixture = Fixture::new(StubState {
            replies: ACTIONS[1..]
                .iter()
                .map(|&action| (action, Reply::Text(" \nnull\t\n".into())))
                .collect(),
            ..StubState::default()
        });
        fixture.run(&options(), stop()).unwrap();
        fixture.assert_actions(&ACTIONS);
    }

    #[test]
    fn method_timeouts_are_bounded_and_mutation_timeouts_restore_once() {
        for (index, &action) in ACTIONS.iter().enumerate() {
            let fixture = Fixture::new(StubState {
                delays: BTreeMap::from([(action, CALL_TIMEOUT + Duration::from_millis(200))]),
                ..StubState::default()
            });
            let started = Instant::now();
            let error = format!("{:#}", fixture.run(&options(), stop()).unwrap_err());
            let elapsed = started.elapsed();
            let expected = if action == "snapshot" {
                HELP.to_owned()
            } else {
                format!("wallpaper {action} failed")
            };
            assert!(error.contains(&expected), "{action}: {error}");
            assert!(
                elapsed >= CALL_TIMEOUT,
                "{action} failed before its timeout: {elapsed:?}"
            );
            assert!(
                elapsed < CALL_TIMEOUT * 3,
                "{action} exceeded its bounded call/cleanup budget: {elapsed:?}"
            );
            let mut actions = ACTIONS[..=index].to_vec();
            if index > 0 && action != "restore" {
                actions.push("restore");
            }
            fixture.assert_actions(&actions);
        }
    }

    #[test]
    fn connections_without_a_sufficient_timeout_are_rejected_before_calls() {
        let fixture = Fixture::new(StubState::default());
        for timeout in [None, Some(CALL_TIMEOUT + Duration::from_millis(1))] {
            let builder = connection::Builder::address(fixture.bus.address.as_str()).unwrap();
            let connection = match timeout {
                Some(timeout) => builder.method_timeout(timeout),
                None => builder,
            }
            .build()
            .unwrap();
            let error = run_connected(connection, URI, &options(), stop()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("requires a bounded method timeout"),
                "{error:#}"
            );
            fixture.assert_actions(&[]);
        }
    }

    #[test]
    fn cleanup_failure_is_not_retried_and_preserves_the_primary_error() {
        for primary_failure in [false, true] {
            for reply in [Reply::Error, Reply::Text("undefined".into())] {
                let cleanup_error = match &reply {
                    Reply::Error => "stub restore rejected",
                    Reply::Text(_) => "invalid restore acknowledgement",
                };
                let mut replies = BTreeMap::from([("restore", reply)]);
                if primary_failure {
                    replies.insert("apply", Reply::Error);
                }
                let fixture = Fixture::new(StubState {
                    replies,
                    ..StubState::default()
                });
                let error = format!("{:#}", fixture.run(&options(), stop()).unwrap_err());
                assert!(error.contains(cleanup_error), "{error}");
                if primary_failure {
                    assert!(error.contains("stub apply rejected"), "{error}");
                    assert!(error.contains("wallpaper apply failed"), "{error}");
                    assert!(error.contains("rollback also failed"), "{error}");
                    fixture.assert_actions(&["snapshot", "switch", "apply", "restore"]);
                } else {
                    fixture.assert_actions(&ACTIONS);
                }
            }
        }
    }
}
