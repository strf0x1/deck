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
use deck_runtime::{Runtime, State, Vm};
use futures::channel::mpsc::TryRecvError;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const READY: Duration = Duration::from_secs(60);

#[test]
fn vm_task_runs_attaches_reattaches_stops_and_destroys() {
    let Some(_) = std::env::var_os(deck_runtime::vm::IMAGE_ENV) else {
        eprintln!("skipping: {} is not set", deck_runtime::vm::IMAGE_ENV);
        return;
    };
    for key in [deck_runtime::vm::JOURNAL_ENV, deck_runtime::vm::KRUN_ENV, deck_runtime::vm::EXECUTABLE_ENV] {
        assert!(std::env::var_os(key).is_some(), "the VM test needs {key}");
    }

    let root = std::env::temp_dir().join(format!("deck-vm-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(root.join("project/README.md"), "vm test\n").unwrap();
    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let spec = Spec { argv: vec!["construct".into(), "acp".into()], cwd: root.join("project") };

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
/// `CONSTRUCT_BASE_URL` must point at construct-dev's scripted server in the
/// test process.
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
        "CONSTRUCT_BASE_URL",
        "OPENAI_API_KEY",
    ] {
        assert!(std::env::var_os(key).is_some(), "the prompt test needs {key}");
    }
    let root = std::env::temp_dir().join(format!("deck-vm-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    let data = root.join("data");
    let paths = Paths::for_data(&data).unwrap();
    let spec = Spec { argv: vec!["construct".into(), "acp".into()], cwd: root.join("project") };

    let runtime = open(&data, &paths);
    let id = runtime.create(spec).unwrap();
    let mut cleanup = Cleanup { data: data.clone(), paths: paths.clone(), id: id.clone(), armed: true };
    let launched = Instant::now();
    runtime.start(&id).unwrap();
    let stream = runtime.attach(&id).unwrap();
    let (client, mut events) = Client::over(PathBuf::from("/workspace"), stream.reader, stream.writer).unwrap();
    let deadline = Instant::now() + READY;
    loop {
        match events.try_recv() {
            Ok(Inbound::Ready { session_id, .. }) => {
                println!("launch to ACP ready: {:?}", launched.elapsed());
                runtime.session(&id, &session_id).unwrap();
                break;
            }
            Ok(Inbound::Failed(reason)) => panic!("ACP failed: {reason}"),
            Ok(_) => {}
            Err(TryRecvError::Closed) => panic!("stream closed before Ready"),
            Err(TryRecvError::Empty) if Instant::now() >= deadline => panic!("no Ready"),
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    let prompted = Instant::now();
    let turn_deadline = prompted + Duration::from_secs(20);
    client.prompt("read /etc/hostname").unwrap();
    loop {
        match events.try_recv() {
            Ok(Inbound::Permission { request, options, .. }) => {
                let allowed = options.iter().find(|o| o.allows()).expect("an allow option");
                client.answer(&request, Some(&allowed.id)).unwrap();
                println!("permission asked and answered: {:?}", prompted.elapsed());
            }
            // Construct advertises liveAttach, so the turn's end is the
            // authoritative `_construct/turn` state, not a prompt response.
            Ok(Inbound::Turn { state: TurnState::Idle { stop_reason }, .. }) => {
                println!("prompt round trip: {:?} ({stop_reason:?})", prompted.elapsed());
                break;
            }
            Ok(Inbound::Failed(reason)) => panic!("ACP failed: {reason}"),
            Ok(_) => {}
            Err(TryRecvError::Closed) => panic!("stream closed during the turn"),
            Err(TryRecvError::Empty) if Instant::now() >= turn_deadline => panic!("turn did not finish"),
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    drop(client);
    runtime.stop(&id).unwrap();
    runtime.destroy(&id).unwrap();
    cleanup.armed = false;
    let _ = std::fs::remove_dir_all(&root);
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
    let stream = runtime.attach(id).unwrap();
    let (client, mut events) = if attach {
        let session = runtime.task(id).unwrap().session_id.unwrap();
        Client::attach(PathBuf::from("/workspace"), stream.reader, stream.writer, session).unwrap()
    } else {
        Client::over(PathBuf::from("/workspace"), stream.reader, stream.writer).unwrap()
    };
    let deadline = Instant::now() + READY;
    loop {
        match events.try_recv() {
            Ok(Inbound::Ready { agent, session_id, .. }) => {
                drop(client);
                // The relay notices the closed peer on its own poll tick.
                std::thread::sleep(Duration::from_millis(150));
                return (agent, session_id);
            }
            Ok(Inbound::Failed(reason)) => panic!("ACP failed: {reason}"),
            Ok(_) => {}
            Err(TryRecvError::Closed) => panic!("the ACP stream closed before Ready"),
            Err(TryRecvError::Empty) if Instant::now() >= deadline => panic!("no ACP Ready within {READY:?}"),
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
