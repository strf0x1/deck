#![cfg(unix)]

#[path = "../../deck-runtime/tests/support/holders.rs"]
mod holders;

use deck_runtime::{DaemonClient, Spec, State};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    data: PathBuf,
    daemon: Child,
    holders: holders::Holders,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.holders.stop();
        if let Ok(client) = DaemonClient::connect(&self.data) {
            let _ = client.shutdown();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn task_resume_runs_through_the_cli_and_rejects_invalid_requests() {
    let data = std::env::temp_dir().join(format!("deck-task-cli-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let daemon = Command::new(deck_runtime::daemon_client::sibling_binary("deckd").unwrap())
        .arg("--data")
        .arg(&data)
        .env("DECK_CREDENTIALS", data.join("credentials.json"))
        .env_remove("CONSTRUCT_BASE_URL")
        .env_remove("CONSTRUCT_MODEL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { holders: holders::Holders::new(&data), daemon, data };
    let deadline = Instant::now() + Duration::from_secs(15);
    let client = loop {
        if let Ok(client) = DaemonClient::connect(&fixture.data) {
            break client;
        }
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    let spec = Spec { argv: vec!["cat".into()], cwd: fixture.data.clone(), ..Default::default() };
    let task = client.run(spec).unwrap().task;
    client.stop(&task.id).unwrap();
    let list = Command::new(env!("CARGO_BIN_EXE_deck"))
        .args(["task", "list"])
        .env("DECK_DATA", &fixture.data)
        .output()
        .unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    let listing = String::from_utf8(list.stdout).unwrap();
    let row = listing.lines().find(|line| line.starts_with(&task.id)).unwrap();
    let fields: Vec<_> = row.split('\t').collect();
    assert_eq!(&fields[2..], &["task", "local", "stopped", "-", "-"]);
    client.shutdown().unwrap();
    drop(client);
    assert!(fixture.daemon.wait().unwrap().success());

    let deck = env!("CARGO_BIN_EXE_deck");
    let run = |args: &[&str]| {
        Command::new(deck)
            .args(args)
            .env("DECK_DATA", &fixture.data)
            .env("DECK_CREDENTIALS", fixture.data.join("credentials.json"))
            .env_remove("CONSTRUCT_BASE_URL")
            .env_remove("CONSTRUCT_MODEL")
            .output()
            .unwrap()
    };
    let output = run(&["task", "resume", &task.id]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), format!("resumed task {}", task.id));
    assert_eq!(DaemonClient::connect(&fixture.data).unwrap().task(&task.id).unwrap().state, State::Running);

    let output = run(&["task", "list"]);
    assert!(output.status.success());
    let listing = String::from_utf8(output.stdout).unwrap();
    let row = listing.lines().find(|line| line.starts_with(&task.id)).unwrap();
    let fields: Vec<_> = row.split('\t').collect();
    assert_eq!(&fields[2..5], &["task", "local", "running"]);
    assert_eq!(fields[6], "-", "the local PID names its supervisor, not its agent");
    assert!(listing.contains("Idle deadlines and keep-running settings are not available yet."));

    let output = run(&["task", "resume", &task.id]);
    assert!(!output.status.success(), "a running task cannot be resumed");
    assert!(!run(&["task", "resume", "missing-task"]).status.success());
    let output = run(&["task", "resume"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: deck task list | resume <id>"));
}
