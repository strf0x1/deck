//! The runtime contract against the local backend, with real processes:
//! `cat` stands in for an agent that answers on its stream, `sh` for one that
//! exits, ignores SIGTERM, or leaves children. Each test gets its own data
//! root, so records from one never reach another.

#![cfg(unix)]

use deck_runtime::{Failure, Local, Notice, Runtime, Spec, State};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::executor::block_on;
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A hung process fails the test instead of hanging the run.
fn hang_guard() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        eprintln!("timed out");
        std::process::exit(1);
    });
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("deck-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(argv: &[&str]) -> Spec {
    Spec { argv: argv.iter().map(|s| s.to_string()).collect(), cwd: std::env::temp_dir(), project: None, github: None }
}

fn sh(script: &str) -> Spec {
    spec(&["sh", "-c", script])
}

#[expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]
fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[expect(clippy::unwrap_used, reason = "test fixture setup must succeed")]
fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[expect(clippy::unwrap_used, reason = "a test helper: a closed channel is the failure")]
fn next(rx: &mut UnboundedReceiver<Notice>) -> State {
    block_on(rx.next()).unwrap().state
}

#[expect(clippy::unwrap_used, reason = "a test helper")]
fn open(dir: &Scratch) -> Runtime {
    Runtime::open(&dir.0, Local).unwrap().with_grace(Duration::from_millis(300))
}

#[test]
fn a_task_records_the_root_commit_of_its_checkout() {
    hang_guard();
    let dir = Scratch::new("project");
    std::fs::create_dir_all(&dir.0).unwrap();
    let repo = dir.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.name", "Ada Lovelace"]);
    git(&repo, &["config", "user.email", "ada@example.com"]);
    std::fs::write(repo.join("README.md"), "one\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);
    let root = git_output(&repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let rt = open(&dir);
    let id = rt.create(Spec { argv: vec!["cat".into()], cwd: repo, project: None, github: None }).unwrap();
    let expected = format!("git:{root}");
    assert_eq!(rt.task(&id).unwrap().spec.project.as_deref(), Some(expected.as_str()));
}

#[test]
fn start_attach_stop_is_recorded_in_order() {
    hang_guard();
    let dir = Scratch::new("order");
    let rt = open(&dir);
    let mut notices = rt.subscribe();

    let id = rt.create(spec(&["cat"])).unwrap();
    rt.start(&id).unwrap();
    let mut stream = rt.attach(&id).unwrap();
    stream.writer.write_all(b"hello\n").unwrap();
    stream.writer.flush().unwrap();
    let mut line = String::new();
    stream.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "hello\n", "the stream reaches the agent and back");
    assert!(rt.attach(&id).is_err(), "a local agent's stdio is taken once");

    rt.stop(&id).unwrap();
    assert_eq!(next(&mut notices), State::Created);
    assert_eq!(next(&mut notices), State::Running);
    let State::Stopped { clean: true, reason } = next(&mut notices) else { panic!("not a clean stop") };
    assert_eq!(reason, "killed by signal 15");
    assert_eq!(rt.task(&id).unwrap().state, State::Stopped { clean: true, reason }, "the record agrees");
    assert!(rt.stop(&id).is_err(), "a stopped task is not running");
}

#[test]
fn session_pointer_is_durable_idempotent_and_retained_on_resume() {
    hang_guard();
    let dir = Scratch::new("session");
    let rt = open(&dir);
    let id = rt.create(spec(&["cat"])).unwrap();
    assert!(rt.session(&id, "session-7").is_err());
    rt.start(&id).unwrap();
    let before = rt.task(&id).unwrap();
    assert!(rt.session(&id, "").is_err());
    assert!(rt.session(&id, "  ").is_err());
    rt.session(&id, "session-7").unwrap();
    let log = dir.0.join("tasks").join(&id).join("events.jsonl");
    let once = std::fs::read(&log).unwrap();
    rt.session(&id, "session-7").unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), once, "repeated session ids are not appended");
    let running = rt.task(&id).unwrap();
    assert_eq!(
        (running.state, running.pid, running.session_id),
        (State::Running, before.pid, Some("session-7".into()))
    );
    rt.stop(&id).unwrap();
    assert!(rt.session(&id, "session-7").is_err());
    drop(rt);
    let rt = open(&dir);
    assert_eq!(rt.task(&id).unwrap().session_id.as_deref(), Some("session-7"));
    rt.resume(&id).unwrap();
    assert_eq!(rt.task(&id).unwrap().session_id.as_deref(), Some("session-7"));
    rt.session(&id, "session-8").unwrap();
    assert_eq!(rt.task(&id).unwrap().session_id.as_deref(), Some("session-8"));
    rt.stop(&id).unwrap();
}

#[test]
fn a_process_ignoring_sigterm_is_killed_after_the_grace() {
    hang_guard();
    let dir = Scratch::new("grace");
    let rt = open(&dir);
    let id = rt.create(sh("trap '' TERM; echo up; while :; do sleep 0.05; done")).unwrap();
    rt.start(&id).unwrap();
    let mut stream = rt.attach(&id).unwrap();
    let mut line = String::new();
    stream.reader.read_line(&mut line).unwrap(); // the trap is set
    let began = Instant::now();
    rt.stop(&id).unwrap();
    assert!(began.elapsed() >= Duration::from_millis(300), "SIGKILL waited for the grace");
    let State::Stopped { clean: true, reason } = rt.task(&id).unwrap().state else { panic!("not stopped") };
    assert!(reason.starts_with("killed by signal 9, after"), "{reason}");
}

#[test]
fn an_exit_nobody_asked_for_is_an_unclean_stop() {
    hang_guard();
    let dir = Scratch::new("exit");
    let rt = open(&dir);
    let mut notices = rt.subscribe();
    let id = rt.create(sh("exit 3")).unwrap();
    rt.start(&id).unwrap();
    assert_eq!(next(&mut notices), State::Created);
    assert_eq!(next(&mut notices), State::Running);
    assert_eq!(next(&mut notices), State::Stopped { clean: false, reason: "exited with code 3".into() });
    assert!(rt.stop(&id).is_err(), "nothing is running");
    // No automatic restart: it stays stopped until someone resumes it.
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(rt.task(&id).unwrap().state, State::Stopped { clean: false, .. }));
}

#[test]
fn the_agents_children_stop_with_it() {
    hang_guard();
    let dir = Scratch::new("group");
    let rt = open(&dir);
    let id = rt.create(sh("sleep 30 & echo $!; wait")).unwrap();
    rt.start(&id).unwrap();
    let mut stream = rt.attach(&id).unwrap();
    let mut line = String::new();
    stream.reader.read_line(&mut line).unwrap();
    let child: i32 = line.trim().parse().unwrap();
    rt.stop(&id).unwrap();
    // Reparented and reaped by init once it dies; until then it is a zombie.
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: kill(2) with signal 0 only checks that the pid exists.
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(Instant::now() < deadline, "the agent's child {child} outlived the stop");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn reopening_turns_a_dead_holders_task_into_an_unclean_stop() {
    hang_guard();
    let dir = Scratch::new("reconcile");
    let held_elsewhere = {
        let rt = open(&dir);
        let gone = rt.create(spec(&["cat"])).unwrap();
        rt.start(&gone).unwrap();
        let other = rt.create(spec(&["cat"])).unwrap();
        rt.start(&other).unwrap();
        // The runtime goes away without stopping anything, as a crash would.
        // Its processes go with it (the local backend lives in-process).
        drop(rt);
        (gone, other)
    };
    let (gone, other) = held_elsewhere;
    // Another live holder still has `other`.
    let store = deck_runtime::record::Store::open(&dir.0).unwrap();
    let lock = store.lock(&other).unwrap().unwrap();
    assert_eq!(store.load(&other).unwrap().state, State::Running, "the crash left it running on record");

    let rt = open(&dir);
    assert_eq!(
        rt.task(&gone).unwrap().state,
        State::Stopped { clean: false, reason: "it was not running when the runtime opened".into() }
    );
    assert_eq!(rt.task(&other).unwrap().state, State::Running, "a held task is left alone");
    assert!(rt.resume(&other).is_err());
    drop(lock);
    rt.resume(&gone).unwrap();
    rt.stop(&gone).unwrap();
}

#[test]
fn run_reuses_a_resting_task_of_the_same_spec() {
    hang_guard();
    let dir = Scratch::new("reuse");
    let rt = open(&dir);
    let (first, _stream) = rt.run(spec(&["cat"])).unwrap();
    rt.stop(&first).unwrap();
    let (again, _stream) = rt.run(spec(&["cat"])).unwrap();
    assert_eq!(again, first, "a stopped task of this spec is resumed");
    let (busy, _stream) = rt.run(spec(&["cat"])).unwrap();
    assert_ne!(busy, first, "a running one is not shared");
    let (other, _stream) = rt.run(spec(&["cat", "-u"])).unwrap();
    assert_ne!(other, first);
    assert_eq!(rt.tasks().unwrap().len(), 3);
    for id in [first, busy, other] {
        rt.stop(&id).unwrap();
    }
}

#[test]
fn a_failed_start_is_recorded_and_retried_in_place() {
    hang_guard();
    let dir = Scratch::new("startfail");
    let rt = open(&dir);
    let bad = spec(&["/nonexistent/agent"]);
    assert!(rt.run(bad.clone()).is_err());
    assert!(rt.run(bad).is_err());
    let tasks = rt.tasks().unwrap();
    assert_eq!(tasks.len(), 1, "a retry reuses the failed task");
    let State::Failed { failure: Failure::StartFailed(why) } = &tasks[0].state else { panic!("{:?}", tasks[0].state) };
    assert!(why.contains("/nonexistent/agent"), "{why}");
}

#[test]
fn destroy_stops_a_running_task_and_is_idempotent() {
    hang_guard();
    let dir = Scratch::new("destroy");
    let rt = open(&dir);
    let id = rt.create(spec(&["cat"])).unwrap();
    rt.start(&id).unwrap();
    rt.destroy(&id).unwrap();
    assert_eq!(rt.task(&id).unwrap().state, State::Destroyed);
    rt.destroy(&id).unwrap();
    assert!(rt.resume(&id).is_err());
    assert!(rt.run(spec(&["cat"])).map(|(new, _)| new != id).unwrap(), "a destroyed task is never reused");
}

#[test]
fn an_unrecorded_holder_shuts_down_instead_of_leaving_an_orphan() {
    use std::io::{BufRead as _, BufReader};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    hang_guard();
    let dir = Scratch::new("unrecorded-holder");
    let store = deck_runtime::record::Store::open(&dir.0).unwrap();
    let task = store.create("local", spec(&["cat"])).unwrap();
    let paths = deck_runtime::paths::Paths::for_data(&dir.0).unwrap();
    std::fs::create_dir_all(paths.directory.join("tasks")).unwrap();
    let socket = paths.task(&task.id);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.0.join("tasks").join(&task.id).join("lock"))
        .unwrap();
    file.try_lock().unwrap();
    let fd = file.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_deckd"));
    command
        .arg("--holder")
        .arg(&dir.0)
        .arg(&task.id)
        .arg(&socket)
        .arg(fd.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls between fork and exec. This fixture
    // passes the same inherited lock descriptor as the daemon backend.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(file);
    let mut header = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut header).unwrap();
    let identity: serde_json::Value = serde_json::from_str(&header).unwrap();
    assert_eq!(identity["Ok"]["holder"]["pid"], child.id());
    assert!(store.lock(&task.id).unwrap().is_none(), "the child owns the inherited lock");
    // Simulate deckd dying after the startup reply but before Started is durable.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("unrecorded holder did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(store.load(&task.id).unwrap().state, State::Created);
    assert!(store.lock(&task.id).unwrap().is_some());
    assert!(!socket.exists());
    assert!(!socket.with_extension("holder").exists());
    std::fs::remove_dir_all(paths.directory).unwrap();
}

mod supervision {
    use super::*;
    use deck_runtime::{Backend, Exited, Instance, Stream, Task, record::RecordLock};
    use std::sync::{Arc, Mutex};

    struct Deferred {
        callbacks: Arc<Mutex<Vec<Exited>>>,
        fail_stop: bool,
    }
    struct DeferredInstance {
        pid: u32,
        fail_stop: bool,
        _lock: RecordLock,
    }
    #[expect(clippy::unwrap_used, reason = "test callback queue must stay available")]
    impl Backend for Deferred {
        fn name(&self) -> &'static str {
            "deferred"
        }
        fn start(&mut self, _task: &Task, lock: RecordLock, exited: Exited) -> anyhow::Result<Box<dyn Instance>> {
            let mut callbacks = self.callbacks.lock().unwrap();
            callbacks.push(exited);
            Ok(Box::new(DeferredInstance { pid: callbacks.len() as u32, fail_stop: self.fail_stop, _lock: lock }))
        }
        fn destroy(&mut self, _task: &Task) -> anyhow::Result<()> {
            Ok(())
        }
    }
    impl Instance for DeferredInstance {
        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }
        fn attach(&mut self) -> anyhow::Result<Stream> {
            anyhow::bail!("not used by supervision tests")
        }
        fn stop(&mut self, _grace: Duration) -> anyhow::Result<String> {
            if std::mem::take(&mut self.fail_stop) {
                anyhow::bail!("shutdown not confirmed");
            }
            Ok("confirmed stop".into())
        }
    }

    #[test]
    fn a_runtime_leaves_another_backends_task_alone() {
        let dir = Scratch::new("other-backend");
        let local = Runtime::open(&dir.0, Local).unwrap();
        let id = local.create(spec(&["cat"])).unwrap();
        local.start(&id).unwrap();
        drop(local); // the local backend's Drop reaps the child and frees the lock

        let callbacks = Arc::new(Mutex::new(vec![]));
        let other = Runtime::open(&dir.0, Deferred { callbacks, fail_stop: false }).unwrap();
        assert_eq!(
            other.task(&id).unwrap().state,
            State::Running,
            "another backend must not reconcile or stop this task"
        );
        assert!(other.destroy(&id).is_err(), "another backend must not delete its files");

        let local = Runtime::open(&dir.0, Local).unwrap();
        assert!(matches!(local.task(&id).unwrap().state, State::Stopped { clean: false, .. }));
        local.destroy(&id).unwrap();
    }

    #[test]
    fn an_old_exit_callback_cannot_stop_a_resumed_task() {
        let dir = Scratch::new("delayed-exit");
        let callbacks = Arc::new(Mutex::new(vec![]));
        let rt = Runtime::open(&dir.0, Deferred { callbacks: callbacks.clone(), fail_stop: false }).unwrap();
        let id = rt.create(spec(&["agent"])).unwrap();
        rt.start(&id).unwrap();
        rt.stop(&id).unwrap();
        rt.resume(&id).unwrap();
        let old = callbacks.lock().unwrap().remove(0);
        old("old holder exited".into());
        let task = rt.task(&id).unwrap();
        assert_eq!((task.state, task.pid), (State::Running, Some(2)));
        let current = callbacks.lock().unwrap().remove(0);
        current("current holder exited".into());
        assert_eq!(
            rt.task(&id).unwrap().state,
            State::Stopped { clean: false, reason: "current holder exited".into() }
        );
    }

    #[test]
    fn destroy_retries_a_failed_stop_and_releases_the_lock() {
        let dir = Scratch::new("retry-stop");
        let rt = Runtime::open(&dir.0, Deferred { callbacks: Arc::new(Mutex::new(vec![])), fail_stop: true }).unwrap();
        let id = rt.create(spec(&["agent"])).unwrap();
        rt.start(&id).unwrap();
        assert!(rt.stop(&id).is_err());
        let failed = rt.task(&id).unwrap();
        assert!(matches!(failed.state, State::Failed { failure: Failure::ShutdownUnconfirmed(_) }));
        assert_eq!(failed.pid, Some(1));
        assert!(rt.resume(&id).is_err());
        assert!(deck_runtime::record::Store::open(&dir.0).unwrap().lock(&id).unwrap().is_none());
        rt.destroy(&id).unwrap();
        assert_eq!(rt.task(&id).unwrap().state, State::Destroyed);
        assert!(deck_runtime::record::Store::open(&dir.0).unwrap().lock(&id).unwrap().is_some());
    }
}
