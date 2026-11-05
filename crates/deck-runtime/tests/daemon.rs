//! The daemon contract through its real binary and Unix control socket.
//! Each test owns a data root and daemon, so hard crashes, idle shutdown and
//! concurrent clients exercise process ownership without sharing state.

#![cfg(unix)]

use deck_runtime::{DaemonClient, Failure, Spec, State, paths::Paths};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_millis(150);

struct Daemon {
    data: PathBuf,
    paths: Paths,
    child: Child,
    /// The `DECK_CREDENTIALS` file this daemon reads, if any.
    credentials: Option<PathBuf>,
}

#[expect(clippy::unwrap_used, reason = "test fixture setup and process observation must succeed")]
impl Daemon {
    fn new(name: &str) -> (Self, DaemonClient) {
        Self::with_credentials(name, None)
    }

    fn data(name: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let data = std::env::temp_dir().join(format!(
            "deck-daemon-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&data).unwrap();
        data
    }

    /// A daemon whose only credential store is `credentials`, a file the
    /// caller wrote at 0600.
    fn with_credentials(name: &str, credentials: Option<PathBuf>) -> (Self, DaemonClient) {
        let data = Self::data(name);
        let paths = Paths::for_data(&data).unwrap();
        let child = Self::spawn(&data, credentials.as_deref());
        let mut daemon = Self { data, paths, child, credentials };
        let client = daemon.connect();
        (daemon, client)
    }

    fn spawn(data: &std::path::Path, credentials: Option<&std::path::Path>) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_deckd"));
        command
            .arg("--data")
            .arg(data)
            .arg("--idle-ms")
            .arg(IDLE.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        match credentials {
            Some(file) => command.env("DECK_CREDENTIALS", file),
            // Never the developer's own store from the test's environment.
            None => command.env("DECK_CREDENTIALS", data.join("no-credentials.json")),
        };
        command.spawn().unwrap()
    }

    fn connect(&mut self) -> DaemonClient {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(client) = DaemonClient::connect(&self.data) {
                return client;
            }
            assert!(self.child.try_wait().unwrap().is_none(), "daemon exited before opening its socket");
            assert!(Instant::now() < deadline, "daemon did not open its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn spec(&self, argv: &[&str]) -> Spec {
        Spec { argv: argv.iter().map(|arg| (*arg).into()).collect(), cwd: self.data.clone() }
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn restart(&mut self) -> DaemonClient {
        self.child = Self::spawn(&self.data, self.credentials.as_deref());
        self.connect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
        let _ = std::fs::remove_dir_all(&self.paths.directory);
    }
}

fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "condition was not observed before timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[expect(clippy::unwrap_used, reason = "test process observation must succeed")]
fn running(pid: u32) -> bool {
    // SAFETY: signal zero only queries whether the process exists.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    // An orphan can briefly remain a zombie until init reaps it. It has
    // already stopped executing and cannot retain a writer to task data.
    let output = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
    let state = String::from_utf8(output.stdout).unwrap();
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

#[test]
fn a_second_daemon_on_the_same_data_root_is_refused() {
    let (mut daemon, client) = Daemon::new("singleton");
    let output = Command::new(env!("CARGO_BIN_EXE_deckd"))
        .arg("--data")
        .arg(&daemon.data)
        .arg("--idle-ms")
        .arg("150")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("already running for this data root"), "{error}");
    assert!(client.tasks().unwrap().is_empty(), "the first daemon still answers");
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn idle_exit_waits_for_control_clients_and_idle_subscriptions() {
    let (mut daemon, client) = Daemon::new("idle");
    std::thread::sleep(IDLE * 3);
    assert!(daemon.child.try_wait().unwrap().is_none(), "a connected control client keeps deckd alive");
    let notices = client.subscribe().unwrap();
    drop(client);
    std::thread::sleep(IDLE * 3);
    assert!(daemon.child.try_wait().unwrap().is_none(), "a quiet subscription keeps deckd alive");
    drop(notices);
    assert!(daemon.wait_exit().success(), "dropping an idle subscription permits shutdown");
    assert!(!daemon.paths.control().exists(), "idle exit removes the socket");
}

#[test]
fn control_and_acp_disconnect_leave_the_task_running() {
    let (mut daemon, client) = Daemon::new("detach");
    let spec = daemon.spec(&["cat"]);
    let run = client.run(spec.clone()).unwrap();
    assert!(run.started);
    let mut stream = client.attach(&run.task.id).unwrap();
    stream.writer.write_all(b"whole line\n").unwrap();
    stream.writer.flush().unwrap();
    let mut line = String::new();
    stream.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "whole line\n");
    stream.writer.close();
    drop(stream);
    drop(client);
    std::thread::sleep(IDLE * 3);
    assert!(daemon.child.try_wait().unwrap().is_none(), "a running task prevents idle exit");
    assert!(running(run.task.pid.unwrap()));
    let client = daemon.connect();
    let reopened = client.run(spec).unwrap();
    assert!(!reopened.started);
    assert_eq!(reopened.task.id, run.task.id);
    assert_eq!(reopened.task.state, State::Running);
    assert_eq!(client.tasks().unwrap().len(), 1);
    client.stop(&run.task.id).unwrap();
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn task_operations_and_state_notices_cross_the_control_socket() {
    let (mut daemon, client) = Daemon::new("operations");
    let mut notices = client.subscribe().unwrap();
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    assert_eq!(task.state, State::Created);
    let started = client.start(&task.id).unwrap();
    assert_eq!(started.state, State::Running);
    client.session(&task.id, "saved-session").unwrap();
    assert_eq!(client.task(&task.id).unwrap().session_id.as_deref(), Some("saved-session"));
    client.stop(&task.id).unwrap();
    let resumed = client.resume(&task.id).unwrap();
    assert_eq!(resumed.state, State::Running);
    assert_eq!(resumed.session_id.as_deref(), Some("saved-session"));
    client.destroy(&task.id).unwrap();
    client.destroy(&task.id).unwrap();
    assert_eq!(client.task(&task.id).unwrap().state, State::Destroyed);
    let mut states = Vec::new();
    eventually(|| {
        while let Ok(notice) = notices.try_recv() {
            assert_eq!(notice.task, task.id);
            states.push(notice.state);
        }
        states.last() == Some(&State::Destroyed)
    });
    assert_eq!(states[0], State::Created);
    assert_eq!(states[1], State::Running);
    assert_eq!(states.iter().filter(|state| matches!(state, State::Stopped { clean: true, .. })).count(), 2);
    drop(notices);
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn rejected_start_and_resume_leave_a_running_tasks_holder_intact() {
    let (mut daemon, client) = Daemon::new("rejected-launch");
    let run = client.run(daemon.spec(&["cat"])).unwrap();
    let mut stream = client.attach(&run.task.id).unwrap();
    for line in ["baseline\n", "after-start\n", "after-resume\n"] {
        match line {
            "after-start\n" => assert!(client.start(&run.task.id).is_err()),
            "after-resume\n" => assert!(client.resume(&run.task.id).is_err()),
            _ => {}
        }
        stream.writer.write_all(line.as_bytes()).unwrap();
        stream.writer.flush().unwrap();
        let mut echoed = String::new();
        stream.reader.read_line(&mut echoed).unwrap();
        assert_eq!(echoed, line, "a rejected lifecycle request must retain the existing ACP stream");
        let task = client.task(&run.task.id).unwrap();
        assert_eq!(task.state, State::Running);
        assert_eq!(task.pid, run.task.pid);
    }
    assert!(running(run.task.pid.unwrap()));
    client.stop(&run.task.id).unwrap();
    drop(stream);
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn concurrent_run_requests_find_one_task() {
    let (mut daemon, client) = Daemon::new("concurrent");
    let spec = daemon.spec(&["cat"]);
    let barrier = Arc::new(Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|_| {
            let data = daemon.data.clone();
            let spec = spec.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let client = DaemonClient::connect(&data).unwrap();
                barrier.wait();
                client.run(spec).unwrap()
            })
        })
        .collect();
    let runs: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert_eq!(runs.iter().filter(|run| run.started).count(), 1);
    assert!(runs.iter().all(|run| run.task.id == runs[0].task.id));
    assert_eq!(client.tasks().unwrap().len(), 1);
    client.stop(&runs[0].task.id).unwrap();
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn a_missing_agent_is_a_failed_start_without_a_started_event() {
    let (mut daemon, client) = Daemon::new("missing-agent");
    let spec = daemon.spec(&["/deck-test/no-such-agent"]);
    assert!(client.run(spec.clone()).is_err());
    let tasks = client.tasks().unwrap();
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    let State::Failed { failure: Failure::StartFailed(reason) } = &task.state else {
        panic!("missing agent must fail before running: {:?}", task.state);
    };
    assert!(reason.contains("/deck-test/no-such-agent"), "{reason}");
    assert_eq!(task.pid, None);
    assert!(client.run(spec).is_err());
    assert_eq!(client.tasks().unwrap().len(), 1, "retry reuses the failed record");
    let log = std::fs::read_to_string(daemon.data.join("tasks").join(&task.id).join("events.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = log.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["event"], "created");
    assert_eq!(events[1]["event"], "failed");
    assert_eq!(events[2]["event"], "failed");
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn killing_a_holder_ends_its_agent_and_tools_and_reconciles_without_restart() {
    let (mut daemon, client) = Daemon::new("holder-crash");
    let run = client.run(daemon.spec(&["sh", "-c", "sleep 60 & echo $! > tool; wait"])).unwrap();
    let holder = run.task.holder.clone().unwrap();
    let mut tool = 0;
    eventually(|| {
        tool = std::fs::read_to_string(daemon.data.join("tool")).unwrap_or_default().trim().parse().unwrap_or(0);
        tool != 0
    });
    // SAFETY: this fixture owns this task's still-running holder.
    assert_eq!(unsafe { libc::kill(holder.pid as i32, libc::SIGKILL) }, 0);
    eventually(|| matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: false, .. }));
    assert!(!running(tool));
    assert!(!running(run.task.pid.unwrap()));
    assert!(!running(holder.pid));
    assert!(!holder.socket.exists());
    assert!(!holder.socket.with_extension("holder").exists());
    let store = deck_runtime::record::Store::open(&daemon.data).unwrap();
    assert!(store.lock(&run.task.id).unwrap().is_some());
    std::thread::sleep(IDLE * 2);
    assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: false, .. }));
    client.destroy(&run.task.id).unwrap();
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn adoption_does_not_attach_or_interrupt_an_existing_acp_stream() {
    let (mut daemon, client) = Daemon::new("live-stream");
    let run = client.run(daemon.spec(&["cat"])).unwrap();
    let mut stream = client.attach(&run.task.id).unwrap();
    stream.writer.write_all(b"before crash\n").unwrap();
    let mut line = String::new();
    stream.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "before crash\n");
    daemon.child.kill().unwrap();
    daemon.wait_exit();
    drop(client);
    stream.writer.write_all(b"during downtime\n").unwrap();
    line.clear();
    stream.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "during downtime\n");
    let client = daemon.restart();
    stream.writer.write_all(b"after adoption\n").unwrap();
    line.clear();
    stream.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "after adoption\n");
    let holder = run.task.holder.unwrap();
    client.destroy(&run.task.id).unwrap();
    assert!(!running(holder.pid));
    assert!(!holder.socket.exists());
    assert!(!holder.socket.with_extension("holder").exists());
    assert!(deck_runtime::record::Store::open(&daemon.data).unwrap().lock(&run.task.id).unwrap().is_some());
    drop(stream);
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn killing_deckd_preserves_agents_and_tools_then_adopts_without_restarting() {
    let (mut daemon, client) = Daemon::new("crash");
    let spec = daemon.spec(&["sh", "-c", "trap '' TERM; sleep 60 & printf '%s %s\\n' \"$$\" \"$!\" > pids; wait"]);
    let run = client.run(spec.clone()).unwrap();
    client.session(&run.task.id, "crash-session").unwrap();
    let mut pids = Vec::new();
    eventually(|| {
        pids = std::fs::read_to_string(daemon.data.join("pids"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
            .collect();
        pids.len() == 2
    });
    let helper = run.task.pid.unwrap();
    assert!(running(helper));
    assert!(pids.iter().all(|&pid| running(pid)));
    daemon.child.kill().unwrap();
    assert!(!daemon.wait_exit().success());
    drop(client);
    let holder = run.task.holder.clone().unwrap();
    assert!(running(holder.pid));
    assert!(running(helper));
    assert!(pids.iter().all(|&pid| running(pid)));
    assert!(deck_runtime::record::Store::open(&daemon.data).unwrap().lock(&run.task.id).unwrap().is_none());
    let store = deck_runtime::record::Store::open(&daemon.data).unwrap();
    assert_eq!(store.load(&run.task.id).unwrap().state, State::Running, "the crash cannot append a clean stop");
    let client = daemon.restart();
    let adopted = client.task(&run.task.id).unwrap();
    assert_eq!(adopted.state, State::Running);
    assert_eq!(adopted.pid, Some(helper));
    assert_eq!(adopted.holder, Some(holder.clone()));
    assert_eq!(adopted.session_id.as_deref(), Some("crash-session"));
    let again = client.run(spec).unwrap();
    assert!(!again.started);
    assert_eq!(again.task.id, run.task.id);
    assert_eq!(again.task.pid, Some(helper));
    assert_eq!(client.tasks().unwrap().len(), 1);
    let log = std::fs::read_to_string(daemon.data.join("tasks").join(&run.task.id).join("events.jsonl")).unwrap();
    assert_eq!(log.lines().filter(|line| line.contains("\"event\":\"adopted\"")).count(), 1);
    client.stop(&run.task.id).unwrap();
    assert!(!running(holder.pid));
    assert!(!running(helper));
    assert!(pids.iter().all(|&pid| !running(pid)));
    assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: true, .. }));
    assert!(!holder.socket.exists());
    assert!(!holder.socket.with_extension("holder").exists());

    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn malformed_control_lines_and_traversal_ids_are_refused_without_stopping_deckd() {
    let (mut daemon, client) = Daemon::new("bad-input");
    let mut socket = UnixStream::connect(daemon.paths.control()).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    let mut input = BufReader::new(socket.try_clone().unwrap());
    for request in ["not json\n", "{\"op\":\"missing\"}\n", "{\"op\":\"task\",\"id\":\"../outside\"}\n"] {
        socket.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        input.read_line(&mut reply).unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert!(value["error"].is_string(), "{reply}");
    }
    assert!(client.task("../outside").is_err());
    assert!(client.attach("../outside").is_err());
    assert!(client.tasks().unwrap().is_empty());
    // A cut-off final request belongs only to this connection.
    socket.write_all(b"{\"op\":").unwrap();
    drop(input);
    drop(socket);
    assert!(client.tasks().unwrap().is_empty());
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn an_unrecorded_startup_group_must_stop_before_a_retry_can_launch() {
    use std::os::unix::process::CommandExt as _;
    let (mut daemon, client) = Daemon::new("pending-launch");
    let task = client.create(daemon.spec(&["sh", "-c", "echo launched > launched; exec cat"])).unwrap();
    let mut previous = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
    let path = daemon.data.join("tasks").join(&task.id).join("holder.json");
    // This is the durable identity left by a holder that died before its
    // startup reply reached deckd. The group still has an executing member.
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "holder": { "pid": previous.id(), "socket": daemon.paths.task(&task.id) },
            "agent": previous.id(),
        }))
        .unwrap(),
    )
    .unwrap();
    let result = client.start(&task.id);
    let alive = running(previous.id());
    previous.kill().unwrap();
    previous.wait().unwrap();
    assert!(result.is_err());
    assert!(alive, "reconciliation never signals an unverified group");
    assert!(!daemon.data.join("launched").exists(), "no second writer was launched");
    assert!(path.exists(), "failed cleanup keeps the ownership record for the next retry");
    let resumed = client.resume(&task.id).unwrap();
    assert_eq!(resumed.state, State::Running);
    eventually(|| daemon.data.join("launched").exists());
    client.destroy(&task.id).unwrap();
    assert!(!path.exists());
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn stop_and_destroy_close_advertised_sessions_with_or_without_an_attached_window() {
    let script = r#"
import json,sys
for line in sys.stdin:
    v=json.loads(line)
    if v.get('method')=='initialize':
        r={'agentCapabilities':{'sessionCapabilities':{'close':{}}}}
    elif v.get('method')=='session/close':
        with open(sys.argv[1], 'w') as f: f.write(v['params']['sessionId'])
        r={}
    else:
        r={}
    print(json.dumps({'id':v.get('id'),'result':r}), flush=True)
"#;
    for attached in [false, true] {
        let (mut daemon, client) = Daemon::new(if attached { "close-attached" } else { "close-detached" });
        let marker = daemon.data.join("closed");
        let run = client.run(daemon.spec(&["python3", "-u", "-c", script, marker.to_str().unwrap()])).unwrap();
        client.session(&run.task.id, "saved-session").unwrap();
        let mut window = attached.then(|| client.attach(&run.task.id).unwrap());
        if let Some(window) = &mut window {
            window.writer.write_all(b"{}\n").unwrap();
            let mut line = String::new();
            window.reader.read_line(&mut line).unwrap();
        }
        if attached {
            client.stop(&run.task.id).unwrap();
            assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: true, .. }));
        } else {
            client.destroy(&run.task.id).unwrap();
            assert_eq!(client.task(&run.task.id).unwrap().state, State::Destroyed);
        }
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "saved-session");
        assert!(!running(run.task.pid.unwrap()));
        assert!(!daemon.paths.task(&run.task.id).exists());
        drop(window);
        drop(client);
        assert!(daemon.wait_exit().success());
    }
}

#[test]
fn an_unresponsive_close_does_not_prevent_confirmed_stop() {
    let (mut daemon, client) = Daemon::new("close-timeout");
    let run = client.run(daemon.spec(&["sleep", "30"])).unwrap();
    client.session(&run.task.id, "saved-session").unwrap();
    let start = Instant::now();
    client.stop(&run.task.id).unwrap();
    assert!(start.elapsed() < WAIT);
    assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: true, .. }));
    assert!(!running(run.task.pid.unwrap()));
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn stop_still_terminates_an_agent_with_a_blocked_acp_input_writer() {
    let (mut daemon, client) = Daemon::new("close-blocked-input");
    let run = client.run(daemon.spec(&["sleep", "30"])).unwrap();
    client.session(&run.task.id, "saved-session").unwrap();
    let mut stream = client.attach(&run.task.id).unwrap();
    let sender = std::thread::spawn(move || {
        let mut line = vec![b'x'; 512 * 1024];
        line.push(b'\n');
        let _ = stream.writer.write_all(&line);
    });
    // The holder forwards more than a pipe can hold to a non-reading agent.
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    client.stop(&run.task.id).unwrap();
    assert!(start.elapsed() < WAIT);
    assert!(!running(run.task.pid.unwrap()));
    sender.join().unwrap();
    drop(client);
    assert!(daemon.wait_exit().success());
}

fn ask(destination: &str) -> deck_runtime::approvals::Ask {
    deck_runtime::approvals::Ask {
        gateway: "model".into(),
        operation: "chat_completions".into(),
        destination: destination.into(),
        credential: Some("OPENAI_API_KEY".into()),
        details: deck_runtime::approvals::Details::default(),
        reason: None,
    }
}

#[test]
fn a_watch_lease_holds_the_request_for_the_window_decision() {
    use deck_runtime::approvals::{Decision, Scope, State};
    let (mut daemon, client) = Daemon::new("approval-hold");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let _watch = client.watch(&task.id).unwrap();
    let holder = daemon.connect();
    let decider = daemon.connect();
    let asked = {
        let id = task.id.clone();
        std::thread::spawn(move || holder.request_approval(&id, ask("other")))
    };
    eventually(|| client.approvals(&task.id).unwrap().len() == 1);
    let item = client.approvals(&task.id).unwrap().remove(0);
    let decided = decider.decide(&item.id, Decision::Task).unwrap();
    assert_eq!(decided.state, State::Allowed(Scope::Task));
    assert_eq!(asked.join().unwrap().unwrap().state, State::Allowed(Scope::Task));
    let again = client.request_approval(&task.id, ask("other")).unwrap();
    assert_eq!(again.state, State::Allowed(Scope::Task));
    client.stop(&task.id).unwrap();
    assert!(client.grants(&task.id).unwrap().is_empty(), "stopping the task ends its grants");
}

#[test]
fn without_a_lease_a_new_scope_is_answered_without_waiting() {
    use deck_runtime::approvals::{Decision, Scope, State};
    let (mut daemon, client) = Daemon::new("approval-fail-fast");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let decider = daemon.connect();
    let started = Instant::now();
    let item = client.request_approval(&task.id, ask("other")).unwrap();
    assert_eq!(item.state, State::Pending);
    assert!(started.elapsed() < Duration::from_secs(2), "no window is attached, so there is nothing to hold for");
    decider.decide(&item.id, Decision::Once).unwrap();
    assert_eq!(client.request_approval(&task.id, ask("other")).unwrap().state, State::Allowed(Scope::Once));
    assert_eq!(client.request_approval(&task.id, ask("other")).unwrap().state, State::Pending);
    client.stop(&task.id).unwrap();
}

#[test]
fn task_grants_survive_a_daemon_restart_and_do_not_widen_other_tasks() {
    use deck_runtime::approvals::{Decision, Scope, State};
    let (mut daemon, client) = Daemon::new("approval-durable");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let item = client.request_approval(&task.id, ask("other")).unwrap();
    client.decide(&item.id, Decision::Task).unwrap();
    assert!(daemon.data.join("tasks").join(&task.id).join("grants.json").exists());
    // The running task keeps the daemon alive, so crash it rather than
    // waiting for idle shutdown; the holder and its grant must survive.
    drop(client);
    daemon.child.kill().unwrap();
    daemon.wait_exit();
    let client = daemon.restart();
    assert_eq!(client.grants(&task.id).unwrap().len(), 1, "the running task keeps its grant across a daemon restart");
    assert_eq!(client.request_approval(&task.id, ask("other")).unwrap().state, State::Allowed(Scope::Task));
    let other = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&other.id).unwrap();
    assert_eq!(client.request_approval(&other.id, ask("other")).unwrap().state, State::Pending);
    client.stop(&other.id).unwrap();
    client.stop(&task.id).unwrap();
}

#[test]
fn revoking_a_task_grant_is_persisted_and_audited_before_the_reply() {
    use deck_runtime::approvals::{Decision, State};
    let (daemon, client) = Daemon::new("grant-revoke-audit");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let mut request = ask("revoked.example:443");
    request.gateway = "web".into();
    request.operation = "connect".into();
    request.credential = None;
    let item = client.request_approval(&task.id, request.clone()).unwrap();
    client.decide(&item.id, Decision::Task).unwrap();
    let grant = client.grants(&task.id).unwrap().remove(0);

    client.revoke_grant(&task.id, &grant.id).unwrap();
    assert!(client.grants(&task.id).unwrap().is_empty());
    assert!(!daemon.data.join("tasks").join(&task.id).join("grants.json").exists());
    let audit = std::fs::read_to_string(daemon.data.join("approvals/audit.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = audit.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "grant_revoked");
    assert_eq!(events[0]["grant"], serde_json::to_value(&grant).unwrap());
    assert_eq!(client.request_approval(&task.id, request).unwrap().state, State::Pending);
    client.stop(&task.id).unwrap();
}

#[test]
fn the_access_panel_lists_its_project_and_a_project_revoke_removes_matching_task_grants() {
    use deck_runtime::approvals::{Decision, Scope};
    let (daemon, client) = Daemon::new("access-project");
    let first = client.create(daemon.spec(&["cat"])).unwrap();
    let peer = client.create(daemon.spec(&["cat"])).unwrap();
    let mut other_spec = daemon.spec(&["cat"]);
    other_spec.cwd = daemon.data.join("other-project");
    std::fs::create_dir_all(&other_spec.cwd).unwrap();
    let other = client.create(other_spec).unwrap();
    let request = |destination: &str| {
        let mut request = ask(destination);
        request.gateway = "web".into();
        request.operation = "connect".into();
        request.credential = None;
        request
    };
    for task in [&first, &peer, &other] {
        let item = client.request_approval(&task.id, request("example.com:443")).unwrap();
        client.decide(&item.id, Decision::Task).unwrap();
    }
    let item = client.request_approval(&peer.id, request("kept.example:443")).unwrap();
    client.decide(&item.id, Decision::Task).unwrap();
    client.web_profile(&first.id, None, "custom", &["example.com:443".into()], false).unwrap();
    let listed = client.access_grants(&first.id).unwrap();
    assert_eq!(listed.len(), 2);
    let project_rule = listed.iter().find(|grant| grant.scope == Scope::Project).unwrap();
    assert_eq!(client.access_grants(&other.id).unwrap().len(), 1, "another project cannot list the rule");
    assert!(client.revoke_access_grant(&other.id, &project_rule.id).is_err(), "another project cannot revoke it");
    client.revoke_access_grant(&first.id, &project_rule.id).unwrap();
    assert!(client.access_grants(&first.id).unwrap().is_empty());
    let peer_grants = client.access_grants(&peer.id).unwrap();
    assert_eq!(peer_grants.len(), 1);
    assert_eq!(peer_grants[0].destination, "kept.example:443");
    assert_eq!(client.access_grants(&other.id).unwrap().len(), 1);
    let reopened = deck_runtime::approvals::Boundary::open(&daemon.data).unwrap();
    assert!(reopened.grants(&first.id).unwrap().is_empty(), "removal is persisted");
    assert_eq!(reopened.grants(&peer.id).unwrap().len(), 1);
    let audit = std::fs::read_to_string(daemon.data.join("approvals/audit.jsonl")).unwrap();
    assert_eq!(audit.lines().count(), 3, "the project rule and two matching task grants are audited");
}

#[test]
fn a_project_rule_outlives_the_task_and_the_daemon() {
    use deck_runtime::approvals::{Decision, Scope, State};
    let (mut daemon, client) = Daemon::new("approval-project");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let item = client.request_approval(&task.id, ask("other")).unwrap();
    client.decide(&item.id, Decision::Project).unwrap();
    client.stop(&task.id).unwrap();
    drop(client);
    assert!(daemon.wait_exit().success());
    let client = daemon.restart();
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    assert_eq!(client.request_approval(&task.id, ask("other")).unwrap().state, State::Allowed(Scope::Project));
    client.stop(&task.id).unwrap();
}

#[test]
fn the_gateways_approval_request_shape_is_accepted() {
    let (daemon, client) = Daemon::new("gateway-wire");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let line = serde_json::json!({
        "op": "approval",
        "task": task.id,
        "gateway": "model",
        "operation": "chat_completions",
        "destination": "other",
        "credential": "OPENAI_API_KEY",
        "details": { "method": "POST", "path": "/v1/chat/completions", "size": 42 }
    });
    let mut socket = UnixStream::connect(daemon.paths.control()).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    writeln!(socket, "{line}").unwrap();
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let approval = &reply["ok"];
    assert_eq!(approval["destination"], "other");
    assert_eq!(approval["state"], "pending");
    assert_eq!(approval["details"]["size"], 42, "the gateway's details object is the queue's");
    assert_eq!(approval["credential"], "OPENAI_API_KEY");
    assert!(client.approvals(&task.id).unwrap().iter().any(|item| item.id == approval["id"]));
    client.stop(&task.id).unwrap();
}

#[test]
fn a_status_wait_and_withdrawal_cross_the_control_socket() {
    let (daemon, client) = Daemon::new("approval-status");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let item = client.request_approval(&task.id, ask("other")).unwrap();

    // A window that attaches after the item exists still sees it.
    let (_watch, mut rx) = client.watch(&task.id).unwrap();
    let snapshot = loop {
        match rx.try_recv() {
            Ok(item) => break item,
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    assert_eq!((snapshot.id.as_str(), snapshot.waiters), (item.id.as_str(), 0));

    // The gateway's long poll is one control call that the daemon holds.
    let control = daemon.paths.control();
    let (asked_task, asked_id) = (task.id.clone(), item.id.clone());
    let waited = std::thread::spawn(move || {
        let mut socket = UnixStream::connect(control).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        writeln!(
            socket,
            "{}",
            serde_json::json!({
                "op": "approval_wait", "task": asked_task, "id": asked_id, "wait_ms": 5000
            })
        )
        .unwrap();
        let mut reply = String::new();
        BufReader::new(socket).read_line(&mut reply).unwrap();
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()
    });
    let heard = Instant::now() + WAIT;
    loop {
        if client.approval_status(&task.id, &item.id).unwrap().unwrap().waiters == 1 {
            break;
        }
        assert!(Instant::now() < heard, "the wait was never visible");
        std::thread::sleep(Duration::from_millis(10));
    }
    let heard = Instant::now() + WAIT;
    loop {
        match rx.try_recv() {
            Ok(update) if update.waiters == 1 => break,
            Ok(_) => {}
            Err(_) => {}
        }
        assert!(Instant::now() < heard, "the card hears the wait");
        std::thread::sleep(Duration::from_millis(10));
    }

    let withdrawn = client.withdraw(&task.id, &item.id).unwrap();
    assert_eq!(withdrawn.state, deck_runtime::approvals::State::Withdrawn);
    assert_eq!(withdrawn.waiters, 0);
    let outcome = waited.join().unwrap();
    assert_eq!(outcome["ok"]["state"], "withdrawn");
    assert_eq!(outcome["ok"]["waiters"], 0);
    let status = client.approval_status(&task.id, &item.id).unwrap().unwrap();
    assert_eq!(status.state, deck_runtime::approvals::State::Withdrawn);
    assert!(client.approvals(&task.id).unwrap().is_empty());
    assert!(client.grants(&task.id).unwrap().is_empty());
    client.stop(&task.id).unwrap();
}

#[test]
fn model_usage_and_a_live_budget_are_recorded_on_the_task() {
    let (mut daemon, client) = Daemon::new("model-budget");
    let task = client.create(daemon.spec(&["cat"])).unwrap();

    client.model_usage(&task.id, 2, 40).unwrap();
    let recorded = client.task(&task.id).unwrap();
    assert_eq!((recorded.usage.requests, recorded.usage.tokens), (2, 40));

    // A late report from an older gateway must not rewind the totals.
    client.model_usage(&task.id, 1, 10).unwrap();
    let recorded = client.task(&task.id).unwrap();
    assert_eq!((recorded.usage.requests, recorded.usage.tokens), (2, 40));

    client.set_budget(&task.id, Some(4), Some(100)).unwrap();
    let path = daemon.data.join("tasks").join(&task.id).join("budget.json");
    let budget: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(budget, serde_json::json!({ "requests": 4, "tokens": 100 }));

    assert!(client.set_budget("absent", None, None).is_err());
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn a_profile_decision_becomes_project_rules_the_gateway_re_reads() {
    let (daemon, client) = Daemon::new("web-profile");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let project = task.spec.cwd.to_string_lossy().into_owned();
    let destinations = vec!["deb.debian.org:443".to_string(), "security.debian.org:443".to_string()];
    assert_eq!(client.web_profile(&task.id, None, "os-mirrors", &destinations, false).unwrap(), 2);
    assert_eq!(
        client.web_profile(&task.id, None, "os-mirrors", &destinations, false).unwrap(),
        0,
        "approving twice changes nothing"
    );
    // The rules are the file the running web gateway re-reads: web/connect on
    // each destination, for this task's project, with no expiry.
    let rules = std::fs::read_to_string(daemon.data.join("approvals").join("projects.json")).unwrap();
    let rules: serde_json::Value = serde_json::from_str(&rules).unwrap();
    let rules = rules["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 2, "{rules:?}");
    for rule in rules {
        assert_eq!(rule["gateway"], "web");
        assert_eq!(rule["operation"], "connect");
        assert_eq!(rule["scope"], "project");
        assert_eq!(rule["project"], project.as_str());
        assert!(rule.get("expires_ms").is_none(), "a project rule has no expiry: {rule}");
    }
    // Revoking removes exactly the named destination and leaves its sibling.
    assert_eq!(client.web_profile(&task.id, Some(&project), "os-mirrors", &destinations[..1], true).unwrap(), 1);
    let rules = std::fs::read_to_string(daemon.data.join("approvals").join("projects.json")).unwrap();
    assert!(rules.contains("security.debian.org:443"), "the sibling stays: {rules}");
    assert!(!rules.contains("deb.debian.org:443"), "the revoked destination is gone: {rules}");
    let audit = std::fs::read_to_string(daemon.data.join("approvals/audit.jsonl")).unwrap();
    let event: serde_json::Value = serde_json::from_str(audit.trim()).unwrap();
    assert_eq!(event["event"], "grant_revoked");
    assert_eq!(event["grant"]["project"], project);
    assert_eq!(event["grant"]["destination"], "deb.debian.org:443");
    client.stop(&task.id).unwrap();
}

#[test]
fn a_github_profile_writes_one_rule_per_operation_and_revokes_exactly_them() {
    let (daemon, client) = Daemon::new("github-profile");
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();
    let project = task.spec.cwd.to_string_lossy().into_owned();
    let operations = ["publish".to_string(), "create_pr".into()];
    assert_eq!(client.github_profile(&task.id, None, "strf0x1/deck", &operations, false).unwrap(), 2);
    assert_eq!(
        client.github_profile(&task.id, None, "strf0x1/deck", &operations, false).unwrap(),
        0,
        "approving twice changes nothing"
    );
    let rules = std::fs::read_to_string(daemon.data.join("approvals").join("projects.json")).unwrap();
    let rules: serde_json::Value = serde_json::from_str(&rules).unwrap();
    let rules = rules["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 2, "{rules:?}");
    for rule in rules {
        assert_eq!(rule["gateway"], "github");
        assert_eq!(rule["destination"], "strf0x1/deck");
        assert_eq!(rule["scope"], "project");
        assert_eq!(rule["project"], project.as_str());
        assert!(rule.get("expires_ms").is_none(), "a project rule has no expiry: {rule}");
    }
    // A merge is not an operation this gateway performs, so no rule for it.
    assert_eq!(
        client.github_profile(&task.id, None, "strf0x1/deck", &["merge".to_string()], false).unwrap_err().to_string(),
        "a GitHub profile names publish, create_pr or update_pr"
    );
    // Revoking one operation leaves the other.
    assert_eq!(
        client.github_profile(&task.id, Some(&project), "strf0x1/deck", &["publish".to_string()], true).unwrap(),
        1
    );
    let rules = std::fs::read_to_string(daemon.data.join("approvals").join("projects.json")).unwrap();
    assert!(rules.contains("create_pr"), "the sibling stays: {rules}");
    assert!(!rules.contains("\"publish\""), "the revoked operation is gone: {rules}");
    client.stop(&task.id).unwrap();
}

/// A 0600 credential file holding `pairs`, beside a test's data root.
#[expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]
fn credential_file(name: &str, pairs: &[(&str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Daemon::data(&format!("{name}-store"));
    let file = dir.join("credentials.json");
    let map: serde_json::Map<String, serde_json::Value> =
        pairs.iter().map(|(name, value)| ((*name).to_string(), serde_json::json!(value))).collect();
    std::fs::write(&file, serde_json::to_vec(&map).unwrap()).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    file
}

/// What a VM run writes: a `vmm.json` naming the task's credential set. A
/// local task stands in for the VMM here; the daemon reads only the set.
#[expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]
fn credential_set(daemon: &Daemon, task: &str, names: &[&str]) {
    let config = serde_json::json!({
        "task": task, "data": daemon.data, "lock_fd": -1, "disk": "/dev/null", "journal": "/dev/null",
        "socket": "/tmp/x", "forward": "/tmp/x", "console": "/tmp/x", "guest": "/bin/true", "argv": [],
        "env": [], "vcpus": 1, "ram_mib": 64, "library": "/dev/null",
        "gateway": {
            "executable": "/bin/true", "model_socket": "/tmp/x", "extension_socket": "/tmp/x",
            "model_port": 1026, "extension_port": 1027, "github_scratch": "/tmp/x",
            "upstream": "http://127.0.0.1:1/v1", "models": ["m"], "budget_file": "/tmp/x",
            "secret": "OPENAI_API_KEY", "credentials": names, "control": "/tmp/x", "task": task
        }
    });
    let dir = daemon.data.join("tasks").join(task);
    std::fs::write(dir.join("vmm.json"), serde_json::to_vec(&config).unwrap()).unwrap();
}

/// One raw control line, as the gateway sends it.
#[expect(clippy::unwrap_used, reason = "test process observation must succeed")]
fn control(daemon: &Daemon, line: serde_json::Value) -> serde_json::Value {
    let mut socket = UnixStream::connect(daemon.paths.control()).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(socket, "{line}").unwrap();
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

fn fetch(daemon: &Daemon, task: &str, name: &str) -> serde_json::Value {
    control(daemon, serde_json::json!({ "op": "credential", "task": task, "name": name }))
}

fn code(reply: &serde_json::Value) -> &str {
    reply["error"].as_str().and_then(|error| error.split(':').next()).unwrap_or("")
}

#[test]
fn a_credential_is_served_per_request_only_to_a_running_task_that_names_it() {
    let file = credential_file("broker", &[("OPENAI_API_KEY", "sk-one")]);
    let (daemon, client) = Daemon::with_credentials("broker", Some(file.clone()));
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();

    // No VMM config: this task has no credential set at all.
    assert_eq!(code(&fetch(&daemon, &task.id, "OPENAI_API_KEY")), "deck_credential_refused");
    credential_set(&daemon, &task.id, &["OPENAI_API_KEY"]);
    let served = fetch(&daemon, &task.id, "OPENAI_API_KEY");
    assert_eq!(served["ok"]["value"], "sk-one");
    let fingerprint = served["ok"]["fingerprint"].as_str().unwrap().to_string();
    assert!(fingerprint.starts_with("sha256:") && fingerprint.len() == 19, "{fingerprint}");
    assert_eq!(fetch(&daemon, &task.id, "OPENAI_API_KEY")["ok"]["value"], "sk-one");
    assert_eq!(code(&fetch(&daemon, &task.id, "DECK_GITHUB_TOKEN")), "deck_credential_refused");
    assert_eq!(code(&fetch(&daemon, &task.id, "lowercase")), "invalid credential name");

    // The record names the value once per run, never the value itself.
    let events = std::fs::read_to_string(daemon.data.join("tasks").join(&task.id).join("events.jsonl")).unwrap();
    assert_eq!(events.matches("credential_used").count(), 1, "{events}");
    assert!(events.contains(&fingerprint) && !events.contains("sk-one"), "{events}");

    // A gateway's watch opens with what is already revoked, then hears more.
    let mut watch = UnixStream::connect(daemon.paths.control()).unwrap();
    watch.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(watch, "{}", serde_json::json!({ "op": "credential_watch", "task": task.id })).unwrap();
    let mut watch = BufReader::new(watch);
    let mut line = String::new();
    watch.read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&line).unwrap()["ok"]["revoked"], serde_json::json!([]));

    let revocation = client.revoke_credential("OPENAI_API_KEY").unwrap();
    assert_eq!(revocation.fingerprint, fingerprint);
    line.clear();
    watch.read_line(&mut line).unwrap();
    let pushed: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        (pushed["name"].as_str(), pushed["fingerprint"].as_str()),
        (Some("OPENAI_API_KEY"), Some(&*fingerprint))
    );
    assert_eq!(code(&fetch(&daemon, &task.id, "OPENAI_API_KEY")), "deck_credential_revoked");

    // Rotation: a new value serves at once; the old one stays revoked.
    let rotated = credential_file("broker-rotated", &[("OPENAI_API_KEY", "sk-two")]);
    std::fs::rename(&rotated, &file).unwrap();
    assert_eq!(fetch(&daemon, &task.id, "OPENAI_API_KEY")["ok"]["value"], "sk-two");
    let revoked = std::fs::read_to_string(daemon.data.join("credentials/revoked.json")).unwrap();
    assert!(revoked.contains(&fingerprint) && !revoked.contains("sk-"), "{revoked}");

    std::fs::remove_file(&file).unwrap();
    assert_eq!(code(&fetch(&daemon, &task.id, "OPENAI_API_KEY")), "deck_credential_missing");
    client.stop(&task.id).unwrap();
    std::fs::rename(&rotated, &file).ok();
    assert_eq!(code(&fetch(&daemon, &task.id, "OPENAI_API_KEY")), "deck_credential_refused", "a stopped task");
}

#[test]
fn a_task_lists_only_its_credential_set_by_name_and_fingerprint() {
    let file = credential_file("listing", &[("OPENAI_API_KEY", "sk-one"), ("OTHER_KEY", "sk-other")]);
    let (daemon, client) = Daemon::with_credentials("listing", Some(file));
    let task = client.create(daemon.spec(&["cat"])).unwrap();
    client.start(&task.id).unwrap();

    assert_eq!(client.credentials(&task.id).unwrap(), vec![], "no VMM config: no set");
    credential_set(&daemon, &task.id, &["OPENAI_API_KEY", "DECK_GITHUB_TOKEN"]);
    let listed = client.credentials(&task.id).unwrap();
    assert_eq!(
        listed.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
        ["OPENAI_API_KEY", "DECK_GITHUB_TOKEN"]
    );
    let fingerprint = listed[0].fingerprint.clone().unwrap();
    assert!(fingerprint.starts_with("sha256:") && !listed[0].revoked);
    assert_eq!((listed[1].fingerprint.as_deref(), listed[1].error.as_deref()), (None, Some("no value")));

    client.revoke_credential("OPENAI_API_KEY").unwrap();
    let listed = client.credentials(&task.id).unwrap();
    assert!(listed[0].revoked && listed[0].fingerprint.as_deref() == Some(&*fingerprint));
    // Nothing on the wire carries a value, and a bad task id is refused.
    let raw = control(&daemon, serde_json::json!({ "op": "credentials", "task": task.id }));
    assert!(!raw.to_string().contains("sk-"), "{raw}");
    assert!(client.credentials("../x").is_err());
    client.stop(&task.id).unwrap();
}

#[test]
fn a_credential_file_other_users_can_read_stops_deckd_at_start() {
    use std::os::unix::fs::PermissionsExt as _;
    let file = credential_file("mode", &[("OPENAI_API_KEY", "sk-one")]);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let data = Daemon::data("mode");
    let output = Command::new(env!("CARGO_BIN_EXE_deckd"))
        .arg("--data")
        .arg(&data)
        .env("DECK_CREDENTIALS", &file)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("chmod 600") && !stderr.contains("sk-one"), "{stderr}");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn the_credential_commands_take_values_on_stdin_and_list_only_fingerprints() {
    let data = Daemon::data("cli");
    let file = data.join("credentials.json");
    let deckd = |args: &[&str], stdin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_deckd"))
            .arg("--data")
            .arg(&data)
            .args(args)
            .env("DECK_CREDENTIALS", &file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(deckd(&["credential", "set", "OPENAI_API_KEY"], "sk-from-stdin\n").status.success());
    assert!(deckd(&["credential", "set", "DECK_GITHUB_TOKEN"], "ghp-token").status.success());
    assert!(!deckd(&["credential", "set", "lower"], "x").status.success());
    assert!(!deckd(&["credential", "set", "EMPTY"], "\n").status.success());
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["OPENAI_API_KEY"], "sk-from-stdin", "one trailing newline is not part of the value");

    let list = deckd(&["credential", "list"], "");
    let text = String::from_utf8(list.stdout).unwrap();
    assert!(text.contains("OPENAI_API_KEY\tsha256:") && text.contains("DECK_GITHUB_TOKEN\tsha256:"), "{text}");
    assert!(!text.contains("sk-from-stdin") && !text.contains("ghp-token"), "{text}");

    assert!(deckd(&["credential", "delete", "DECK_GITHUB_TOKEN"], "").status.success());
    let text = String::from_utf8(deckd(&["credential", "list"], "").stdout).unwrap();
    assert!(!text.contains("DECK_GITHUB_TOKEN"), "{text}");
    let _ = std::fs::remove_dir_all(&data);
}
