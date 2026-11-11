//! The lifecycle contract against a real libkrun guest. Skipped unless the
//! VM environment names a guest image, journal template, libkrun and a
//! `deckd` that can run `--vmm` (see docs/vm-spike.md).
//!
//! The design choice: this is the same contract `lifecycle.rs` runs against a
//! child process, not a second set of assertions. A backend that only boots a
//! VM but cannot be adopted or destroyed through the contract has not been
//! proven.

#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test fixture setup and VM observation must succeed")]

use deck_acp::{Client, Inbound, TurnState};
use deck_runtime::lifecycle::Spec;
use deck_runtime::paths::Paths;
use deck_runtime::{DaemonClient, Runtime, State, Vm};
use futures::channel::mpsc::UnboundedReceiver;
use futures::future::{Either, select};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const READY: Duration = Duration::from_secs(60);

/// Block until the next inbound event or the deadline. The old 20 ms tick sat
/// between the agent's line and the test, so launch timings included up to
/// 20 ms of harness latency; this wakes on the event itself.
fn next(events: &mut UnboundedReceiver<Inbound>, deadline: Instant) -> Option<Inbound> {
    let wait = deadline.saturating_duration_since(Instant::now());
    let (fired, fire) = futures::channel::oneshot::channel();
    let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if matches!(cancelled.recv_timeout(wait), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            let _ = fired.send(());
        }
    });
    let outcome = futures::executor::block_on(select(events.recv(), fire));
    drop(cancel);
    watchdog.join().ok();
    match outcome {
        Either::Left((Ok(event), _)) => Some(event),
        Either::Left((Err(_), _)) => panic!("the ACP stream closed"),
        Either::Right(_) => None,
    }
}

#[test]
fn vm_task_runs_attaches_reattaches_stops_and_destroys() {
    let Some(_) = std::env::var_os(deck_runtime::vm::IMAGE_ENV) else {
        eprintln!("skipping: {} is not set", deck_runtime::vm::IMAGE_ENV);
        return;
    };
    for key in [
        deck_runtime::vm::JOURNAL_ENV,
        deck_runtime::vm::KRUN_ENV,
        deck_runtime::vm::EXECUTABLE_ENV,
        deck_runtime::vm::GATEWAY_ENV,
    ] {
        assert!(std::env::var_os(key).is_some(), "the VM test needs {key}");
    }

    let root = std::env::temp_dir().join(format!("deck-vm-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(root.join("project/README.md"), "vm test\n").unwrap();
    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let spec = Spec {
        argv: vec!["construct".into(), "acp".into()],
        cwd: root.join("project"),
        project: None,
        github: None,
        model: None,
    };

    let runtime = open(&data, &paths);
    let id = runtime.create(spec.clone()).unwrap();
    let mut cleanup = Cleanup { data: data.clone(), paths: paths.clone(), id: id.clone(), armed: true };
    runtime.start(&id).unwrap();
    assert_eq!(runtime.task(&id).unwrap().state, State::Running);
    let session = ready(&runtime, &id, false).1;
    runtime.session(&id, &session).unwrap();

    // Drop the runtime without stopping: the VMM is the holder and must
    // survive, and the next runtime adopts it from the record.
    drop(runtime);
    let runtime = open(&data, &paths);
    assert_eq!(runtime.task(&id).unwrap().state, State::Running);
    ready(&runtime, &id, true);

    runtime.stop(&id).unwrap();
    assert!(matches!(runtime.task(&id).unwrap().state, State::Stopped { clean: true, .. }));

    // Resume reuses the same disks; the journal survives the stop.
    runtime.resume(&id).unwrap();
    ready(&runtime, &id, false);
    runtime.stop(&id).unwrap();
    runtime.destroy(&id).unwrap();
    cleanup.armed = false;
    assert_eq!(runtime.task(&id).unwrap().state, State::Destroyed);
    assert!(!data.join("tasks").join(&id).join("disk.img").exists());
    assert!(!data.join("tasks").join(&id).join("journal.img").exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// Run a prompt that needs a tool and a permission answer, and print the
/// timings #16 records. Skipped unless a model endpoint is configured:
/// `CONSTRUCT_BASE_URL` names the provider the host gateway proxies for the
/// task (construct-dev's scripted server in the test process), and
/// `DECK_CREDENTIALS` names a 0600 file holding `OPENAI_API_KEY`, the host
/// key the gateway fetches from `deckd` per request (ADR 0004 §4).
///
/// The task runs under a real `deckd` (the signed `DECK_VM_EXECUTABLE`),
/// because only the daemon hands out credentials. That daemon is started with
/// a decoy `OPENAI_API_KEY` exported, which must reach neither the VMM nor the
/// gateway.
#[test]
#[ignore = "needs a scripted model endpoint; set DECK_VM_MOCK=1"]
fn vm_prompt_and_permission_round_trip() {
    if std::env::var_os("DECK_VM_MOCK").is_none() {
        eprintln!("skipping: DECK_VM_MOCK is not set");
        return;
    }
    for key in [
        deck_runtime::vm::IMAGE_ENV,
        deck_runtime::vm::JOURNAL_ENV,
        deck_runtime::vm::KRUN_ENV,
        deck_runtime::vm::EXECUTABLE_ENV,
        deck_runtime::vm::GATEWAY_ENV,
        "CONSTRUCT_BASE_URL",
        deck_runtime::credentials::FILE_ENV,
    ] {
        assert!(std::env::var_os(key).is_some(), "the prompt test needs {key}");
    }
    let root = std::env::temp_dir().join(format!("deck-vm-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let spec = Spec {
        argv: vec!["construct".into(), "acp".into()],
        cwd: root.join("project"),
        project: None,
        github: None,
        model: None,
    };

    let child = std::process::Command::new(std::env::var_os(deck_runtime::vm::EXECUTABLE_ENV).unwrap())
        .arg("--data")
        .arg(&data)
        .env("OPENAI_API_KEY", "decoy-exported-in-deckd-shell")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    let deckd = loop {
        if let Ok(deckd) = DaemonClient::connect(&data) {
            break deckd;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "deckd exited before opening its socket");
        assert!(Instant::now() < deadline, "deckd did not open its socket");
        std::thread::sleep(Duration::from_millis(20));
    };
    let id = deckd.create(spec).unwrap().id;
    // Declared after the daemon guard, so on a panic the daemon is gone
    // before Cleanup opens the runtime to stop the VMM.
    let mut cleanup = Cleanup { data: data.clone(), paths: paths.clone(), id: id.clone(), armed: true };
    let launched = Instant::now();
    let task = deckd.start(&id).unwrap();
    let started = launched.elapsed();
    let vmm = task.pid.expect("a running VM task has a VMM pid");
    for pid in std::iter::once(vmm).chain(children(vmm)) {
        let environment = environment(pid);
        assert!(
            !environment.contains("OPENAI_API_KEY=") && !environment.contains("DECK_CREDENTIALS="),
            "process {pid} inherited a credential name: {environment}"
        );
    }
    let stream = deckd.attach(&id).unwrap();
    let attached = launched.elapsed();
    let (client, mut events) = Client::over(PathBuf::from("/workspace"), stream.reader, stream.writer).unwrap();
    let deadline = Instant::now() + READY;
    loop {
        let Some(event) = next(&mut events, deadline) else { panic!("no Ready within {READY:?}") };
        match event {
            Inbound::Ready { session_id, .. } => {
                println!(
                    "deckd start: {started:?}; attach: {:?}; launch to ACP ready: {:?}",
                    attached - started,
                    launched.elapsed()
                );
                deckd.session(&id, &session_id).unwrap();
                break;
            }
            Inbound::Failed(reason) => panic!("ACP failed: {reason}"),
            _ => {}
        }
    }
    let prompted = Instant::now();
    let turn_deadline = prompted + Duration::from_secs(20);
    client.prompt("read /etc/hostname").unwrap();
    loop {
        let Some(event) = next(&mut events, turn_deadline) else { panic!("turn did not finish") };
        match event {
            Inbound::Permission { request, options, .. } => {
                let allowed = options.iter().find(|o| o.allows()).expect("an allow option");
                client.answer(&request, Some(&allowed.id)).unwrap();
                println!("permission asked and answered: {:?}", prompted.elapsed());
            }
            // Construct advertises liveAttach, so the turn's end is the
            // authoritative turn notification, not a prompt response.
            Inbound::Turn { state: TurnState::Idle { stop_reason }, .. } => {
                println!("prompt round trip: {:?} ({stop_reason:?})", prompted.elapsed());
                break;
            }
            Inbound::Failed(reason) => panic!("ACP failed: {reason}"),
            _ => {}
        }
    }
    drop(client);
    deckd.stop(&id).unwrap();
    deckd.destroy(&id).unwrap();
    cleanup.armed = false;
    // The record names the value the run used, never the value.
    let events = std::fs::read_to_string(data.join("tasks").join(&id).join("events.jsonl")).unwrap();
    assert!(events.contains("credential_used"), "{events}");
    drop(deckd);
    daemon.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// The test's `deckd`, killed when the test ends however it ends.
struct Daemon(std::process::Child);

impl Daemon {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A process's environment as `ps eww` shows it to the same user.
fn environment(pid: u32) -> String {
    let output = std::process::Command::new("ps").args(["eww", "-o", "command=", "-p", &pid.to_string()]).output();
    output.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default()
}

/// Direct children of `pid` (the VMM's gateway).
fn children(pid: u32) -> Vec<u32> {
    let output = std::process::Command::new("pgrep").args(["-P", &pid.to_string()]).output();
    output
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| line.trim().parse().ok()).collect()
        })
        .unwrap_or_default()
}

/// #30's acceptance run: twenty consecutive launches on fresh tasks, each
/// timed from `runtime.start` to ACP `Ready`. Stops use a short grace because
/// the guest has no power-key handler; that grace is not in the measurement.
#[test]
#[ignore = "twenty VM launches take about a minute; run explicitly"]
fn vm_launch_to_ready_is_event_driven_twenty_times() {
    if std::env::var_os(deck_runtime::vm::IMAGE_ENV).is_none() {
        eprintln!("skipping: {} is not set", deck_runtime::vm::IMAGE_ENV);
        return;
    }
    for key in [deck_runtime::vm::JOURNAL_ENV, deck_runtime::vm::KRUN_ENV, deck_runtime::vm::EXECUTABLE_ENV] {
        assert!(std::env::var_os(key).is_some(), "the launch test needs {key}");
    }
    let root = std::env::temp_dir().join(format!("deck-vm-launch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let spec = Spec {
        argv: vec!["construct".into(), "acp".into()],
        cwd: root.join("project"),
        project: None,
        github: None,
        model: None,
    };
    let runtime = Runtime::open(&data, Vm::from_env(&data, paths.clone()).expect("VM backend configuration"))
        .expect("runtime")
        .with_grace(Duration::from_millis(200));

    let mut launches = Vec::new();
    for run in 1..=20 {
        let id = runtime.create(spec.clone()).unwrap();
        let mut cleanup = Cleanup { data: data.clone(), paths: paths.clone(), id: id.clone(), armed: true };
        let launched = Instant::now();
        runtime.start(&id).unwrap();
        let started = launched.elapsed();
        let (_, _, ready) = ready_timed(&runtime, &id, false, launched);
        println!("launch {run:>2}: runtime.start {started:?}, launch to ACP ready {ready:?}");
        launches.push(ready);
        runtime.stop(&id).unwrap();
        runtime.destroy(&id).unwrap();
        cleanup.armed = false;
    }
    let total = launches.iter().sum::<Duration>();
    let worst = launches.iter().max().unwrap();
    println!("launch to ACP ready over 20 starts: mean {:?}, worst {worst:?}", total / 20);
    let _ = std::fs::remove_dir_all(&root);
}

/// Two projects under one daemon record two GitHub destinations (ADR 0008
/// §3). The policy is resolved once, at create, and survives a runtime reopen.
#[test]
fn vm_tasks_record_their_projects_github_policy() {
    let Some(_) = std::env::var_os(deck_runtime::vm::IMAGE_ENV) else {
        eprintln!("skipping: {} is not set", deck_runtime::vm::IMAGE_ENV);
        return;
    };
    for key in [
        deck_runtime::vm::JOURNAL_ENV,
        deck_runtime::vm::KRUN_ENV,
        deck_runtime::vm::EXECUTABLE_ENV,
        deck_runtime::vm::GATEWAY_ENV,
    ] {
        assert!(std::env::var_os(key).is_some(), "the policy test needs {key}");
    }
    for key in [
        deck_runtime::github::REPOSITORY_ENV,
        deck_runtime::github::BASE_ENV,
        deck_runtime::github::BRANCHES_ENV,
        deck_runtime::github::TOKEN_ENV,
    ] {
        if std::env::var_os(key).is_some() {
            eprintln!("skipping: {key} overrides every task's GitHub policy");
            return;
        }
    }

    let root = std::env::temp_dir().join(format!("deck-vm-policy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut specs = Vec::new();
    for (name, origin) in [("first", "https://github.com/Owner/One.git"), ("second", "git@github.com:Other/Two.git")] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["remote", "add", "origin", origin]);
        specs.push(Spec {
            argv: vec!["construct".into(), "acp".into()],
            cwd: dir,
            project: None,
            github: None,
            model: None,
        });
    }

    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let runtime = open(&data, &paths);
    let first = runtime.create(specs[0].clone()).unwrap();
    let second = runtime.create(specs[1].clone()).unwrap();
    let repository = |id: &str| runtime.task(id).unwrap().spec.github.expect("a recorded policy").repository;
    assert_eq!(repository(&first), "owner/one");
    assert_eq!(repository(&second), "other/two");

    // Both the event log and the human-readable snapshot carry it.
    for (id, expected) in [(&first, "owner/one"), (&second, "other/two")] {
        let snapshot = std::fs::read_to_string(data.join("tasks").join(id).join("task.json")).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(snapshot["spec"]["github"]["repository"], expected, "{snapshot}");
    }

    // The record, not the daemon's memory, is what a restarted runtime reads.
    drop(runtime);
    let runtime = open(&data, &paths);
    assert_eq!(runtime.task(&first).unwrap().spec.github.unwrap().repository, "owner/one");
    assert_eq!(runtime.task(&second).unwrap().spec.github.unwrap().repository, "other/two");
    let _ = std::fs::remove_dir_all(&root);
}

/// #84: the guest checkout is ready to publish (ADR 0008 §4). Skipped unless
/// a scratch GitHub repository and a token are configured. The image must
/// include the test entry (`scripts/vm-image.sh`), and the repository must be
/// public so the guest's unauthenticated `git fetch` through the web gateway
/// works; the token is only for the GitHub operation gateway.
#[test]
fn vm_guest_checkout_is_ready_to_publish() {
    let Some(_) = std::env::var_os(deck_runtime::vm::IMAGE_ENV) else {
        eprintln!("skipping: {} is not set", deck_runtime::vm::IMAGE_ENV);
        return;
    };
    for key in [
        deck_runtime::vm::JOURNAL_ENV,
        deck_runtime::vm::KRUN_ENV,
        deck_runtime::vm::EXECUTABLE_ENV,
        deck_runtime::vm::GATEWAY_ENV,
    ] {
        assert!(std::env::var_os(key).is_some(), "the checkout test needs {key}");
    }
    let Some(repository) = std::env::var("DECK_VM_GITHUB_REPOSITORY").ok().filter(|value| !value.is_empty()) else {
        eprintln!("skipping: DECK_VM_GITHUB_REPOSITORY is not set");
        return;
    };
    let repository = repository.to_ascii_lowercase();
    let Some(credentials) = std::env::var_os(deck_runtime::credentials::FILE_ENV) else {
        eprintln!("skipping: {} is not set", deck_runtime::credentials::FILE_ENV);
        return;
    };
    let store = std::fs::read_to_string(&credentials).expect("the credential file");
    assert!(
        store.contains(deck_runtime::github::TOKEN),
        "the credential file must hold {}",
        deck_runtime::github::TOKEN
    );
    let base = std::env::var("DECK_VM_GITHUB_BASE").unwrap_or_else(|_| "main".into());

    let root = std::env::temp_dir().join(format!("deck-vm-checkout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let project = root.join("project");
    let clone = std::process::Command::new("git")
        .args(["clone", "--quiet", &format!("https://github.com/{repository}.git")])
        .arg(&project)
        .status()
        .unwrap();
    assert!(clone.success(), "cloning the scratch repository {repository}");
    git(&project, &["config", "user.name", "Deck Test"]);
    git(&project, &["config", "user.email", "deck-test@example.com"]);
    std::fs::write(project.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(project.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(project.join("untracked.txt"), "work in progress\n").unwrap();
    git(&project, &["add", ".gitignore"]);
    git(&project, &["commit", "--quiet", "-m", "checkout fixture"]);

    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let policy = deck_runtime::github::Policy {
        repository: repository.clone(),
        base: base.clone(),
        branches: Vec::new(),
        credential: deck_runtime::github::TOKEN.into(),
        approvals: deck_runtime::github::Approvals::OwnBranchPublic,
        identity: deck_runtime::github::Identity { name: "Deck Test".into(), email: "deck-test@example.com".into() },
    };
    let spec = Spec {
        argv: vec!["construct".into(), "acp".into()],
        cwd: project.clone(),
        project: None,
        github: Some(policy),
        model: None,
    };

    let child = std::process::Command::new(std::env::var_os(deck_runtime::vm::EXECUTABLE_ENV).unwrap())
        .arg("--data")
        .arg(&data)
        .env("DECK_VM_GUEST", "/usr/local/bin/deck-guest-checkout-test")
        .env_remove(deck_runtime::github::REPOSITORY_ENV)
        .env_remove(deck_runtime::github::BASE_ENV)
        .env_remove(deck_runtime::github::BRANCHES_ENV)
        .env_remove(deck_runtime::github::TOKEN_ENV)
        .env_remove(deck_runtime::vm::WEB_PROFILES_ENV)
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    let deckd = loop {
        if let Ok(deckd) = DaemonClient::connect(&data) {
            break deckd;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "deckd exited before opening its socket");
        assert!(Instant::now() < deadline, "deckd did not open its socket");
        std::thread::sleep(Duration::from_millis(20));
    };
    let id = deckd.create(spec).unwrap().id;
    let mut cleanup = Cleanup { data: data.clone(), paths: paths.clone(), id: id.clone(), armed: true };
    deckd.start(&id).unwrap();

    let console = data.join("tasks").join(&id).join("console.log");
    let deadline = Instant::now() + Duration::from_secs(180);
    let text = loop {
        let text = std::fs::read_to_string(&console).unwrap_or_default();
        if text.contains("CHECK-DONE") {
            break text;
        }
        assert!(Instant::now() < deadline, "the guest checks did not finish:\n{text}");
        std::thread::sleep(Duration::from_millis(200));
    };
    println!("{text}");
    let branch = format!("deck/{id}");
    for expected in [
        format!("CHECK branch={branch}"),
        format!("CHECK origin=https://github.com/{repository}.git"),
        "CHECK identity=Deck Test <deck-test@example.com>".into(),
        "CHECK env=absent".into(),
        "CHECK untracked=present".into(),
        "CHECK markers=ignored".into(),
        "CHECK fetch=ok".into(),
        "CHECK commit=ok".into(),
        "CHECK commit-author=Deck Test <deck-test@example.com>".into(),
        "CHECK bundle=ok".into(),
        "CHECK publish=ok".into(),
        format!("CHECK publish-branch={branch}"),
    ] {
        assert!(text.contains(&expected), "missing {expected:?} in:\n{text}");
    }
    let remote = std::process::Command::new("git")
        .args(["ls-remote", &format!("https://github.com/{repository}.git"), &format!("refs/heads/{branch}")])
        .output()
        .unwrap();
    assert!(remote.status.success(), "ls-remote failed: {}", String::from_utf8_lossy(&remote.stderr));
    assert!(!remote.stdout.is_empty(), "the publish created the remote branch");

    deckd.stop(&id).unwrap();
    deckd.destroy(&id).unwrap();
    cleanup.armed = false;
    drop(deckd);
    daemon.stop();
    let _ = std::fs::remove_dir_all(&root);
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn open(data: &Path, paths: &Paths) -> Runtime {
    Runtime::open(data, Vm::from_env(data, paths.clone()).expect("VM backend configuration")).expect("runtime")
}

/// A panicking test must not leave a detached VMM and its disks behind; the
/// instance is detached on purpose, so Drop stops and destroys it.
struct Cleanup {
    data: PathBuf,
    paths: Paths,
    id: String,
    armed: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(vm) = Vm::from_env(&self.data, self.paths.clone()) else { return };
        if let Ok(runtime) = Runtime::open(&self.data, vm) {
            let _ = runtime.stop(&self.id);
            let _ = runtime.destroy(&self.id);
        }
    }
}

/// Attach, handshake, and return the agent name and session id.
fn ready(runtime: &Runtime, id: &str, attach: bool) -> (String, String) {
    let (agent, session, _) = ready_timed(runtime, id, attach, Instant::now());
    (agent, session)
}

/// [`ready`] with the time from `launched` to ACP `Ready`.
fn ready_timed(runtime: &Runtime, id: &str, attach: bool, launched: Instant) -> (String, String, Duration) {
    let stream = runtime.attach(id).unwrap();
    let (client, mut events) = if attach {
        let session = runtime.task(id).unwrap().session_id.unwrap();
        Client::attach(PathBuf::from("/workspace"), stream.reader, stream.writer, session).unwrap()
    } else {
        Client::over(PathBuf::from("/workspace"), stream.reader, stream.writer).unwrap()
    };
    let deadline = Instant::now() + READY;
    loop {
        let Some(event) = next(&mut events, deadline) else { panic!("no ACP Ready within {READY:?}") };
        match event {
            Inbound::Ready { agent, session_id, .. } => {
                let elapsed = launched.elapsed();
                drop(client);
                // The relay notices the closed peer on its own poll tick.
                std::thread::sleep(Duration::from_millis(150));
                return (agent, session_id, elapsed);
            }
            Inbound::Failed(reason) => panic!("ACP failed: {reason}"),
            _ => {}
        }
    }
}
