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
}

#[expect(clippy::unwrap_used, reason = "test fixture setup and process observation must succeed")]
impl Daemon {
    fn new(name: &str) -> (Self, DaemonClient) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let data = std::env::temp_dir().join(format!(
            "deck-daemon-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&data).unwrap();
        let paths = Paths::for_data(&data).unwrap();
        let child = Self::spawn(&data);
        let mut daemon = Self { data, paths, child };
        let client = daemon.connect();
        (daemon, client)
    }

    fn spawn(data: &std::path::Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_deckd"))
            .arg("--data")
            .arg(data)
            .arg("--idle-ms")
            .arg(IDLE.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
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
        self.child = Self::spawn(&self.data);
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
