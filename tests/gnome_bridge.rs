#![cfg(target_os = "linux")]

// Compile the production module here to test its private connection boundary without a test API.
use wallbreak::{media, project};
include!("../src/gnome_bridge.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{BTreeMap, VecDeque},
        fs,
        io::{BufRead, BufReader},
        os::unix::ffi::OsStringExt,
        process::{Child, Command, Stdio},
        sync::{Mutex, mpsc},
    };

    const OBJECT: &str = "/io/github/flakesonnix/Wallbreak";
    const SESSION: &str = "12345678-1234-4567-89ab-123456789abc";

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
                });
            let program = program.expect("protocol tests require dbus-daemon; set WALLBREAK_TEST_DBUS_DAEMON to its absolute path");
            assert!(
                !program.as_os_str().is_empty(),
                "empty explicit dbus-daemon path"
            );
            let directory = tempfile::tempdir().unwrap();
            let mut daemon = Command::new(program);
            // Sandboxes without /etc/dbus-1 supply a minimal session config.
            if let Some(config) = std::env::var_os("WALLBREAK_TEST_DBUS_CONFIG") {
                daemon.arg(format!("--config-file={}", config.to_string_lossy()));
            } else {
                daemon.arg("--session");
            }
            let mut child = daemon
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
                .expect(
                    "private dbus-daemon must start, especially in an explicitly selected lane",
                );
            let stdout = child.stdout.take().unwrap();
            let mut bus = Self {
                child,
                address: String::new(),
                _directory: directory,
            };
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
            assert!(
                bus.address.starts_with("unix:"),
                "private bus requires a local Unix address"
            );
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
            assert!(
                self.child.try_wait().unwrap().is_some(),
                "private daemon was not reaped"
            );
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            self.terminate();
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Arguments {
        renderer: String,
        source: String,
        fps: u32,
        fit: String,
        ffmpeg: String,
        output: String,
    }

    struct StubState {
        protocol: u32,
        starts: Vec<Arguments>,
        sessions: Vec<String>,
        senders: Vec<String>,
        states: VecDeque<&'static str>,
        status_error: bool,
        start_delay: Duration,
        status_delay: Duration,
        stop_delay: Duration,
        stop_received: Option<Instant>,
        cleanup_states: VecDeque<&'static str>,
        stop_error: bool,
        stops: usize,
        started: Option<Instant>,
        running: Option<Instant>,
        stopped: Option<Instant>,
        session: String,
        cancel_on_start: Option<Arc<AtomicBool>>,
    }

    impl Default for StubState {
        fn default() -> Self {
            Self {
                protocol: 1,
                starts: Vec::new(),
                sessions: Vec::new(),
                senders: Vec::new(),
                states: VecDeque::from(["running"]),
                status_error: false,
                start_delay: Duration::ZERO,
                status_delay: Duration::ZERO,
                stop_delay: Duration::ZERO,
                stop_received: None,
                cleanup_states: VecDeque::from(["stopping", "stopped"]),
                stop_error: false,
                stops: 0,
                started: None,
                running: None,
                stopped: None,
                session: SESSION.into(),
                cancel_on_start: None,
            }
        }
    }

    struct Stub(Arc<Mutex<StubState>>);

    #[zbus::interface(name = "io.github.flakesonnix.Wallbreak1")]
    impl Stub {
        #[zbus(out_args("protocol"))]
        fn info(&self) -> u32 {
            self.0.lock().unwrap().protocol
        }

        #[allow(clippy::too_many_arguments)]
        #[zbus(out_args("session"))]
        fn start(
            &self,
            renderer: String,
            source: String,
            fps: u32,
            fit: String,
            ffmpeg: String,
            output: String,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> String {
            let mut state = self.0.lock().unwrap();
            state.starts.push(Arguments {
                renderer,
                source,
                fps,
                fit,
                ffmpeg,
                output,
            });
            state.senders.push(header.sender().unwrap().as_str().into());
            state.started = Some(Instant::now());
            if let Some(cancel) = &state.cancel_on_start {
                cancel.store(true, Ordering::Relaxed);
            }
            let delay = state.start_delay;
            let session = state.session.clone();
            drop(state);
            thread::sleep(delay);
            session
        }

        #[zbus(out_args("state", "detail"))]
        fn status(&self, session: String) -> zbus::fdo::Result<(String, String)> {
            let mut state = self.0.lock().unwrap();
            state.sessions.push(session);
            let stopping = state.stops > 0;
            if !stopping && state.status_error {
                return Err(zbus::fdo::Error::Failed(
                    "stub Status transport failure".into(),
                ));
            }
            let states = if stopping {
                &mut state.cleanup_states
            } else {
                &mut state.states
            };
            let current = if states.len() > 1 {
                states.pop_front().unwrap()
            } else {
                states[0]
            };
            if current == "running" {
                state.running.get_or_insert_with(Instant::now);
            }
            if current == "stopped" {
                state.stopped = Some(Instant::now());
            }
            let delay = if stopping {
                Duration::ZERO
            } else {
                state.status_delay
            };
            drop(state);
            thread::sleep(delay);
            Ok((
                current.into(),
                "protocol stub detail; no renderer executed".into(),
            ))
        }

        fn stop(&self, session: String) -> zbus::fdo::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.sessions.push(session);
            state.stops += 1;
            state.stop_received.get_or_insert_with(Instant::now);
            let delay = state.stop_delay;
            let fails = state.stop_error;
            drop(state);
            thread::sleep(delay);
            if fails {
                Err(zbus::fdo::Error::Failed("stub Stop rejected".into()))
            } else {
                Ok(())
            }
        }
    }

    struct Fixture {
        // Connections drop before the owned daemon; its address exists only in memory.
        service: Connection,
        state: Arc<Mutex<StubState>>,
        renderer: PathBuf,
        media: Media,
        _directory: tempfile::TempDir,
        bus: PrivateBus,
    }

    impl Fixture {
        fn new(state: StubState) -> Self {
            let bus = PrivateBus::new();
            let state = Arc::new(Mutex::new(state));
            let service = connection::Builder::address(bus.address.as_str())
                .unwrap()
                .method_timeout(CALL_TIMEOUT)
                .serve_at(OBJECT, Stub(state.clone()))
                .unwrap()
                .name(SERVICE)
                .unwrap()
                .build()
                .unwrap();
            let directory = tempfile::tempdir().unwrap();
            let renderer = directory.path().join("wallbreak-renderer");
            fs::write(
                &renderer,
                b"not executable: only the bridge may launch a renderer",
            )
            .unwrap();
            let source = directory.path().join("original clip #?.mp4");
            fs::write(&source, b"stub media, not decoded").unwrap();
            let media = project::resolve(&source, false).unwrap();
            Self {
                service,
                state,
                renderer,
                media,
                _directory: directory,
                bus,
            }
        }

        fn run(
            &self,
            options: &PlaybackOptions,
            output: Option<&str>,
            stop: Arc<AtomicBool>,
        ) -> Result<()> {
            run_connected(
                self.bus.connection(),
                &self.renderer,
                &self.media,
                options,
                output,
                stop,
            )
        }

        fn assert_cleaned(&self) {
            let state = self.state.lock().unwrap();
            assert!(state.stops >= 1, "controller must call Stop");
            assert!(
                state.stopped.is_some(),
                "controller must await stopped, not just Stop reply"
            );
            assert!(
                state
                    .sessions
                    .iter()
                    .all(|session| session == &state.session)
            );
            let sender = zbus::names::BusName::try_from(state.senders[0].as_str()).unwrap();
            let bus = DBusProxy::new(&self.service).unwrap();
            let deadline = Instant::now() + CALL_TIMEOUT;
            while bus.name_has_owner(sender.clone()).unwrap() {
                assert!(
                    Instant::now() < deadline,
                    "controller connection must close after cleanup"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn options() -> PlaybackOptions {
        PlaybackOptions {
            fps: 37,
            fit: Fit::Contain,
            ffmpeg: "custom ffmpeg --literal #?".into(),
            duration: Some(Duration::from_millis(120)),
        }
    }

    fn cancelled() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn absent_bridge_and_version_mismatch_never_start() {
        let fixture = Fixture::new(StubState {
            protocol: 2,
            ..StubState::default()
        });
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(format!("{error:#}").contains("protocol 2"));
        assert!(error.to_string().contains("wallbreak gnome-extension"));
        assert!(fixture.state.lock().unwrap().starts.is_empty());
        fixture.service.release_name(SERVICE).unwrap();
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("installation/activation are not automatic")
        );
        assert!(fixture.state.lock().unwrap().starts.is_empty());
    }

    #[test]
    fn forwards_original_project_and_caller_choices_and_counts_duration_from_running() {
        let mut fixture = Fixture::new(StubState {
            states: VecDeque::from(["starting", "starting", "starting", "running"]),
            ..StubState::default()
        });
        let project = fixture._directory.path().join("project original.json");
        fs::write(
            &project,
            b"original project; must not use media/preview instead",
        )
        .unwrap();
        fixture.media.project_path = Some(project.clone());
        fixture.media.source_path = project.clone();
        fixture.media.kind = project::MediaKind::Scene;
        fixture
            .run(&options(), Some("DP-1 literal #?"), cancelled())
            .unwrap();
        fixture.assert_cleaned();
        let state = fixture.state.lock().unwrap();
        assert_eq!(
            state.starts,
            vec![Arguments {
                renderer: fixture.renderer.to_str().unwrap().into(),
                source: project.to_str().unwrap().into(),
                fps: 37,
                fit: "contain".into(),
                ffmpeg: "custom ffmpeg --literal #?".into(),
                output: "DP-1 literal #?".into(),
            }]
        );
        assert!(
            state
                .running
                .unwrap()
                .duration_since(state.started.unwrap())
                >= Duration::from_millis(250)
        );
        assert!(
            state
                .stop_received
                .unwrap()
                .duration_since(state.running.unwrap())
                >= options().duration.unwrap()
        );
    }

    #[test]
    fn direct_source_cover_and_default_primary_output() {
        let fixture = Fixture::new(StubState::default());
        let mut options = options();
        options.fps = 60;
        options.fit = Fit::Cover;
        options.duration = Some(Duration::ZERO);
        fixture.run(&options, None, cancelled()).unwrap();
        fixture.assert_cleaned();
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.starts[0].source, fixture.media.path.to_str().unwrap());
        assert_eq!(state.starts[0].output, "");
        assert_eq!(state.starts[0].fit, "cover");
        assert_eq!(state.starts[0].fps, 60);
    }

    #[test]
    fn cancellation_during_start_attempts_stop_and_waits_for_confirmation() {
        let stop = cancelled();
        let fixture = Fixture::new(StubState {
            cancel_on_start: Some(stop.clone()),
            ..StubState::default()
        });
        fixture.run(&options(), Some(""), stop).unwrap();
        fixture.assert_cleaned();
    }

    #[test]
    fn failed_renderer_unknown_state_and_status_error_all_stop() {
        for (state, status_error, expected) in [
            (
                "failed",
                false,
                "GNOME renderer failed: protocol stub detail",
            ),
            ("RUNNING", false, "unknown state"),
            ("running", true, "cannot query GNOME animation Status"),
        ] {
            let fixture = Fixture::new(StubState {
                states: VecDeque::from([state]),
                status_error,
                ..StubState::default()
            });
            let error = fixture.run(&options(), None, cancelled()).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            fixture.assert_cleaned();
        }
    }

    #[test]
    fn status_call_timeout_is_bounded_and_still_stops() {
        let fixture = Fixture::new(StubState {
            status_delay: CALL_TIMEOUT + Duration::from_millis(300),
            ..StubState::default()
        });
        let started = Instant::now();
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(format!("{error:#}").contains("cannot query GNOME animation Status"));
        assert!(started.elapsed() < Duration::from_secs(6));
        fixture.assert_cleaned();
    }

    #[test]
    fn stop_reply_timeout_still_requires_and_accepts_stopped_confirmation() {
        let fixture = Fixture::new(StubState {
            stop_delay: CALL_TIMEOUT + Duration::from_millis(300),
            ..StubState::default()
        });
        let mut options = options();
        options.duration = Some(Duration::ZERO);
        fixture.run(&options, None, cancelled()).unwrap();
        fixture.assert_cleaned();
    }

    #[test]
    fn start_timeout_without_a_token_reports_unknown_and_disconnects_its_sender() {
        let fixture = Fixture::new(StubState {
            start_delay: CALL_TIMEOUT + Duration::from_millis(300),
            ..StubState::default()
        });
        let started = Instant::now();
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cleanup unknown because no session identifier")
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.stops, 0, "no token exists to address Stop");
        assert_eq!(state.starts.len(), 1);
        let sender = zbus::names::BusName::try_from(state.senders[0].as_str()).unwrap();
        let observer = fixture.bus.connection();
        let bus = DBusProxy::new(&observer).unwrap();
        let deadline = Instant::now() + CALL_TIMEOUT;
        while bus.name_has_owner(sender.clone()).unwrap() {
            assert!(
                Instant::now() < deadline,
                "Start timeout must disconnect the controller"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn disconnected_bridge_cannot_claim_cleanup_success() {
        let fixture = Fixture::new(StubState::default());
        let connection = fixture.bus.connection();
        let state = fixture.state.clone();
        let result = thread::scope(|scope| {
            let controller = scope.spawn(|| {
                let mut options = options();
                options.duration = None;
                run_connected(
                    connection,
                    &fixture.renderer,
                    &fixture.media,
                    &options,
                    None,
                    cancelled(),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(3);
            while state.lock().unwrap().running.is_none() {
                assert!(Instant::now() < deadline, "stub did not enter running");
                thread::sleep(Duration::from_millis(10));
            }
            fixture.service.clone().close().unwrap();
            controller.join().unwrap()
        });
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains("cannot query GNOME animation Status"),
            "{error}"
        );
        assert!(error.contains("cleanup unknown"), "{error}");
        assert!(error.contains("Stop also failed"), "{error}");
        assert!(state.lock().unwrap().stopped.is_none());
    }

    #[test]
    fn unknown_cleanup_state_is_not_proof_of_stopped() {
        let fixture = Fixture::new(StubState {
            cleanup_states: VecDeque::from(["gone"]),
            ..StubState::default()
        });
        let error = format!(
            "{:#}",
            fixture.run(&options(), None, cancelled()).unwrap_err()
        );
        assert!(error.contains("cleanup unknown"), "{error}");
        assert!(error.contains("unknown state \"gone\""), "{error}");
        assert!(
            fixture.state.lock().unwrap().stops >= 2,
            "Drop must retry owned cleanup"
        );
    }

    #[test]
    fn starting_has_an_overall_deadline_and_then_stops() {
        let fixture = Fixture::new(StubState {
            states: VecDeque::from(["starting"]),
            ..StubState::default()
        });
        let started = Instant::now();
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not reach running within 25 seconds")
        );
        assert!(started.elapsed() < START_TIMEOUT + Duration::from_secs(1));
        fixture.assert_cleaned();
    }

    #[test]
    fn failed_cleanup_preserves_primary_error_and_has_one_shared_deadline() {
        let fixture = Fixture::new(StubState {
            states: VecDeque::from(["failed"]),
            cleanup_states: VecDeque::from(["stopping"]),
            stop_error: true,
            ..StubState::default()
        });
        let started = Instant::now();
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("GNOME renderer failed"), "{text}");
        assert!(text.contains("cleanup unknown"), "{text}");
        assert!(text.contains("stub Stop rejected"), "{text}");
        assert!(started.elapsed() < CLEANUP_TIMEOUT + Duration::from_secs(1));
        assert!(fixture.state.lock().unwrap().stops >= 1);
        assert!(fixture.state.lock().unwrap().stopped.is_none());
    }

    #[test]
    fn settled_failure_is_not_proof_of_cleanup() {
        let fixture = Fixture::new(StubState {
            states: VecDeque::from(["failed"]),
            cleanup_states: VecDeque::from(["failed"]),
            ..StubState::default()
        });
        let error = format!(
            "{:#}",
            fixture.run(&options(), None, cancelled()).unwrap_err()
        );
        assert!(error.contains("GNOME renderer failed"), "{error}");
        assert!(error.contains("cleanup unknown"), "{error}");
        assert!(error.contains("last status: Some((Failed"), "{error}");
        assert!(fixture.state.lock().unwrap().stops >= 1);
    }

    #[test]
    fn invalid_uuid_is_owned_and_stopped_before_returning_error() {
        let fixture = Fixture::new(StubState {
            session: "not-a-uuid".into(),
            ..StubState::default()
        });
        let error = fixture.run(&options(), None, cancelled()).unwrap_err();
        assert!(error.to_string().contains("invalid session UUID"));
        fixture.assert_cleaned();
    }

    #[test]
    fn drop_keeps_the_sender_alive_until_stop_is_confirmed() {
        let fixture = Fixture::new(StubState::default());
        let connection = fixture.bus.connection();
        let proxy = ShellBridgeProxy::new(&connection).unwrap();
        let session = proxy
            .start(
                fixture.renderer.to_str().unwrap(),
                fixture.media.path.to_str().unwrap(),
                1,
                "cover",
                "ffmpeg",
                "",
            )
            .unwrap();
        let owner = Playback {
            proxy,
            session,
            owned: true,
            cleanup_deadline: None,
        };
        drop(connection);
        drop(owner);
        fixture.assert_cleaned();
    }

    #[test]
    fn validates_paths_fps_and_preview_without_starting_any_process() {
        assert_eq!(
            sibling_renderer(Path::new("/nix/store/example/bin/.wallbreak-wrapped")).unwrap(),
            Path::new("/nix/store/example/bin/wallbreak-renderer")
        );
        let mut fixture = Fixture::new(StubState::default());
        for fps in [0, 61, u32::MAX] {
            let mut options = options();
            options.fps = fps;
            assert!(
                fixture
                    .run(&options, None, cancelled())
                    .unwrap_err()
                    .to_string()
                    .contains("FPS")
            );
        }
        let renderer = fixture.renderer.clone();
        for path in [
            PathBuf::from("relative-renderer"),
            fixture._directory.path().join("missing"),
            fixture
                ._directory
                .path()
                .join(std::ffi::OsString::from_vec(vec![0xff])),
        ] {
            fixture.renderer = path;
            assert!(fixture.run(&options(), None, cancelled()).is_err());
        }
        fixture.renderer = renderer;
        let mut bad_options = options();
        bad_options.ffmpeg = std::ffi::OsString::from_vec(vec![0xff]).into();
        assert!(
            fixture
                .run(&bad_options, None, cancelled())
                .unwrap_err()
                .to_string()
                .contains("FFmpeg path is not UTF-8")
        );
        fixture.media.source_path = fixture
            ._directory
            .path()
            .join(std::ffi::OsString::from_vec(vec![0xff]));
        assert!(
            fixture
                .run(&options(), None, cancelled())
                .unwrap_err()
                .to_string()
                .contains("source path is not UTF-8")
        );
        fixture.media.preview_only = true;
        assert!(
            fixture
                .run(&options(), None, cancelled())
                .unwrap_err()
                .to_string()
                .contains("static preview")
        );
        assert!(fixture.state.lock().unwrap().starts.is_empty());
        // Already-cancelled public playback must not connect to any ambient bus.
        run(
            &fixture.media,
            &options(),
            None,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
    }

    #[test]
    fn canonical_xml_matches_the_typed_stub_by_named_method() {
        let file = Path::new(file!());
        let test_file = if file.is_absolute() {
            file.to_owned()
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(file)
        };
        let path = test_file
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("extensions/wallbreak@flakesonnix.github.io/dbus.xml");
        let canonical =
            fs::read_to_string(path).expect("canonical GNOME bridge contract is required");
        // This contract has only method/arg elements. Compare their complete spelling after
        // whitespace normalization, independent of method ordering; no extra XML dependency.
        fn methods(xml: &str) -> BTreeMap<String, String> {
            let mut quoted = false;
            let compact: String = xml
                .chars()
                .filter(|character| {
                    if *character == '"' {
                        quoted = !quoted;
                    }
                    quoted || !character.is_ascii_whitespace()
                })
                .collect();
            let mut body = compact
                .split_once("<interfacename=\"io.github.flakesonnix.Wallbreak1\">")
                .expect("missing named Wallbreak1 interface")
                .1
                .split_once("</interface>")
                .expect("unterminated interface")
                .0;
            let mut methods = BTreeMap::new();
            while !body.is_empty() {
                let rest = body
                    .strip_prefix("<methodname=\"")
                    .expect("unexpected interface member");
                let (name, rest) = rest.split_once("\">").expect("invalid method name");
                let (args, rest) = rest.split_once("</method>").expect("unterminated method");
                assert!(
                    methods.insert(name.into(), args.into()).is_none(),
                    "duplicate method {name}"
                );
                body = rest;
            }
            methods
        }
        let fixture = Fixture::new(StubState::default());
        let connection = fixture.bus.connection();
        let proxy = ShellBridgeProxy::new(&connection).unwrap();
        let introspection = proxy.inner().introspect().unwrap();
        let expected = methods(&introspection);
        assert_eq!(
            expected.keys().map(String::as_str).collect::<Vec<_>>(),
            ["Info", "Start", "Status", "Stop"]
        );
        assert_eq!(methods(&canonical), expected);
    }
}
