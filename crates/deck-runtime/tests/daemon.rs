//! The daemon contract through its real binary and Unix control socket.
//! Each test owns a data root and daemon, so hard crashes, idle shutdown and
//! concurrent clients exercise process ownership without sharing state.

#![cfg(unix)]

use deck_runtime::{DaemonClient, Failure, Spec, State, paths::Paths};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
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
fn startup_refuses_to_reuse_a_previous_group_that_has_not_stopped() {
    struct StoppedGroup {
        group: u32,
        member: Child,
    }
    impl Drop for StoppedGroup {
        fn drop(&mut self) {
            // SAFETY: this fixture still owns a member of this group.
            unsafe { libc::kill(-(self.group as i32), libc::SIGKILL) };
            let _ = self.member.wait();
        }
    }

    let (mut daemon, client) = Daemon::new("stalled-crash");
    let run = client.run(daemon.spec(&["cat"])).unwrap();
    let helper = run.task.pid.unwrap();
    // A stopped orphan group receives SIGHUP and SIGCONT from the kernel.
    // Keep a test-owned member with a living parent outside this group so
    // the freeze remains in force after deckd dies.
    let member = Command::new("sleep").arg("60").process_group(helper as i32).spawn().unwrap();
    let group = StoppedGroup { group: helper, member };
    // SAFETY: this test owns the helper and its entire process group.
    assert_eq!(unsafe { libc::kill(-(helper as i32), libc::SIGSTOP) }, 0);
    eventually(|| {
        let state = Command::new("ps").args(["-o", "stat=", "-p", &helper.to_string()]).output().unwrap();
        String::from_utf8(state.stdout).unwrap().trim().starts_with('T')
    });
    daemon.child.kill().unwrap();
    assert!(!daemon.wait_exit().success());
    drop(client);
    daemon.child = Command::new(env!("CARGO_BIN_EXE_deckd"))
        .arg("--data")
        .arg(&daemon.data)
        .arg("--idle-ms")
        .arg("150")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    assert!(!daemon.wait_exit().success(), "startup must refuse to reuse a live old writer");
    assert!(started.elapsed() >= Duration::from_secs(2), "startup waits for group teardown before refusing");
    let mut error = String::new();
    daemon.child.stderr.take().unwrap().read_to_string(&mut error).unwrap();
    assert!(error.contains("has not stopped; refusing to reuse it"), "{error}");
    let store = deck_runtime::record::Store::open(&daemon.data).unwrap();
    assert_eq!(store.load(&run.task.id).unwrap().state, State::Running, "no reusable stop is recorded prematurely");
    drop(group);
    eventually(|| !running(helper));
    let client = daemon.restart();
    assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: false, .. }));
    drop(client);
    assert!(daemon.wait_exit().success());
}

#[test]
fn killing_deckd_ends_agents_and_tools_then_reconciles_without_restarting() {
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
    eventually(|| !running(helper) && pids.iter().all(|&pid| !running(pid)));
    let store = deck_runtime::record::Store::open(&daemon.data).unwrap();
    assert_eq!(store.load(&run.task.id).unwrap().state, State::Running, "the crash cannot append a clean stop");
    let client = daemon.restart();
    let stopped = client.task(&run.task.id).unwrap();
    assert!(matches!(stopped.state, State::Stopped { clean: false, .. }));
    assert_eq!(stopped.pid, None);
    assert_eq!(stopped.session_id.as_deref(), Some("crash-session"));
    std::thread::sleep(IDLE * 2);
    assert!(matches!(client.task(&run.task.id).unwrap().state, State::Stopped { clean: false, .. }));
    let again = client.run(spec).unwrap();
    assert!(again.started);
    assert_eq!(again.task.id, run.task.id);
    assert_eq!(again.task.session_id.as_deref(), Some("crash-session"));
    assert_ne!(again.task.pid, Some(helper));
    assert_eq!(client.tasks().unwrap().len(), 1);
    client.stop(&run.task.id).unwrap();
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
