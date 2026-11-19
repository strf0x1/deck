//! A local coordinator's control surface end to end (deck#152): the real
//! `deckd`, its holders, the loopback `deck-gateway --control-listen` and
//! the fake agent as worker. The test plays the coordinator's agent: it reads
//! `DECK_CONTROL_URL` and `DECK_CONTROL_TOKEN` from the coordinator's own
//! environment and drives the surface over HTTP, as Construct's `deck` tool
//! will.

#![cfg(unix)]
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test fixture setup and replies must succeed")]

#[path = "support/holders.rs"]
mod holders;

use deck_runtime::{DaemonClient, Phase, Spec, TaskKind};
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);

struct Daemon {
    data: PathBuf,
    child: Child,
    _holders: holders::Holders,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon(name: &str) -> (Daemon, DaemonClient) {
    daemon_with(name, false)
}

fn daemon_with(name: &str, vm: bool) -> (Daemon, DaemonClient) {
    let data = std::env::temp_dir().join(format!("deck-control-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let gateway =
        deck_runtime::daemon_client::sibling_binary("deck-gateway").expect("build the workspace binaries first");
    let mut command = Command::new(env!("CARGO_BIN_EXE_deckd"));
    if vm {
        let placeholder = data.join("unused-vm-fixture");
        std::fs::write(&placeholder, "").unwrap();
        command
            .env("DECK_VM_IMAGE", &placeholder)
            .env("DECK_VM_JOURNAL", &placeholder)
            .env("DECK_VM_KRUN", &placeholder);
    } else {
        command.env_remove("DECK_VM_IMAGE");
    }
    let child = command
        .arg("--data")
        .arg(&data)
        .args(["--idle-ms", "500"])
        .env("DECK_GATEWAY_EXECUTABLE", gateway)
        .env("DECK_CREDENTIALS", data.join("no-credentials.json"))
        .env_remove("CONSTRUCT_BASE_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let holders = holders::Holders::new(&data);
    eventually(|| DaemonClient::connect(&data).is_ok());
    let client = DaemonClient::connect(&data).unwrap();
    (Daemon { data, child, _holders: holders }, client)
}

#[test]
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
fn an_integration_reserves_its_destination_and_keeps_the_first_published_pin() {
    let (daemon, client) = daemon_with("integrate", true);
    let repo = daemon.data.join("repo");
    git_repo(&repo);
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["remote", "add", "origin", "https://github.com/deck-test/integration.git"])
            .status()
            .unwrap()
            .success()
    );
    let project = client.open_project(&repo, vec!["construct".into(), "acp".into()]).unwrap();
    let spec = Spec {
        argv: project.argv.clone(),
        cwd: repo.clone(),
        project: Some(project.id.clone()),
        backend: Some("local".into()),
        kind: TaskKind::Coordinator,
        ..Default::default()
    };
    let first = client.create(spec.clone()).unwrap();
    let out = daemon.data.join("integration-control-env");
    let mut policy = client.coordinator_policy(&first.id, None).unwrap();
    policy.coordinator_argv = Some(env_stub(&out));
    client.coordinator_policy(&first.id, Some(policy)).unwrap();
    client.destroy(&first.id).unwrap();
    let coordinator = client.run(spec).unwrap().task;
    eventually(|| out.exists());
    let env = std::fs::read_to_string(&out).unwrap();
    let mut lines = env.lines();
    let (url, token) = (lines.next().unwrap(), lines.next());

    let contributor = || {
        client
            .create(Spec {
                argv: project.argv.clone(),
                cwd: repo.clone(),
                project: Some(project.id.clone()),
                backend: Some("vm".into()),
                ..Default::default()
            })
            .unwrap()
    };
    let (a, b) = (contributor(), contributor());
    let branch = format!("deck/{}", a.id);
    let pin = "a".repeat(40);
    client
        .publication(&a.id, deck_runtime::Publication::Branch { branch: branch.clone(), commit: pin.clone() })
        .unwrap();
    let args = json!({
        "operation": "integrate", "destination": { "kind": "new_pull_request" },
        "contributors": [a.id, b.id], "brief": "Resolve both changes."
    });
    let (one, two) = std::thread::scope(|scope| {
        let request = scope.spawn(|| post(url, token, &args));
        let other = post(url, token, &args);
        (request.join().unwrap(), other)
    });
    assert_eq!(one, two);
    assert_eq!(one.0, 200, "{}", one.1);
    let id = one.1["id"].as_str().unwrap();
    let wait = json!({ "operation": "wait", "id": id, "wait_ms": 5000 });
    let read = || {
        let (status, body) = post(url, token, &wait);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["timed_out"], false, "{body}");
        let integration = body["task"]["integration"].clone();
        assert_eq!(integration["phase"], "waiting");
        assert_eq!(integration["contributors"], json!([{ "id": a.id, "commit": pin }, { "id": b.id }]));
        assert!(integration["reason"].as_str().unwrap().contains(&b.id));
        assert_eq!(body["task"]["attention"], "integration_input");
    };
    read();
    let mut different_order = args.clone();
    different_order["contributors"] = json!([b.id, a.id]);
    assert_eq!(code(&post(url, token, &different_order)), (409, "deck_control_busy"));
    assert_eq!(
        code(&post(url, token, &json!({"operation":"prompt","id":id,"text":"Start anyway"}))),
        (409, "deck_control_busy")
    );
    assert!(client.start(id).is_err());
    client.publication(&a.id, deck_runtime::Publication::Branch { branch, commit: "b".repeat(40) }).unwrap();
    assert_eq!(post(url, token, &args), one);
    read();
    client.destroy(id).unwrap();
    let (status, replacement) = post(url, token, &args);
    assert_eq!(status, 200);
    assert_ne!(replacement["id"], id);
    client.destroy(replacement["id"].as_str().unwrap()).unwrap();
    client.stop(&coordinator.id).unwrap();
}

fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "condition was not observed before timeout");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success());
    git(&["init", "--quiet", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(dir.join("README.md"), "one\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "--quiet", "-m", "one"]);
}

/// One request on a fresh connection; the status and the JSON body.
fn post(url: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let body = body.to_string();
    let auth = token.map(|t| format!("authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(stream, "POST /control HTTP/1.1\r\nhost: deck\r\n{auth}content-length: {}\r\n\r\n{body}", body.len())
        .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let status = reply.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap())
}

fn code(reply: &(u16, Value)) -> (u16, &str) {
    (reply.0, reply.1["error"]["code"].as_str().unwrap_or("none"))
}

/// The coordinator's agent: write the control variables it was given, then
/// answer nothing (the test drives the surface itself).
fn env_stub(out: &Path) -> Vec<String> {
    let script = format!(
        "printf '%s\\n%s\\n' \"$DECK_CONTROL_URL\" \"$DECK_CONTROL_TOKEN\" > '{}.tmp' && mv '{0}.tmp' '{0}'; exec cat",
        out.display()
    );
    vec!["sh".into(), "-c".into(), script]
}

fn audit(data: &Path, id: &str) -> Vec<Value> {
    let log = std::fs::read_to_string(data.join("tasks").join(id).join("events.jsonl")).unwrap_or_default();
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["event"] == "control")
        .collect()
}

#[test]
fn a_local_coordinator_drives_its_workers_through_the_token_guarded_surface() {
    let (daemon, client) = daemon("e2e");
    let repo = daemon.data.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let fake =
        deck_runtime::daemon_client::sibling_binary("deck-fake-agent").expect("build the workspace binaries first");
    let agent = vec!["env".to_string(), "DECK_FAKE_DELAY_MS=20".into(), fake.display().to_string()];
    let project = client.open_project(&repo, agent.clone()).unwrap();
    let other = daemon.data.join("other");
    git_repo(&other);
    std::fs::write(other.join("x"), "unrelated root\n").unwrap();

    // The user records the coordinator command (window op), then starts it.
    let spec = Spec {
        argv: vec!["ignored".into()],
        cwd: repo.clone(),
        project: Some(project.id.clone()),
        kind: TaskKind::Coordinator,
        ..Default::default()
    };
    let first = client.create(spec.clone()).unwrap();
    let out = daemon.data.join("control-env");
    let mut settings = client.coordinator_policy(&first.id, None).unwrap();
    settings.coordinator_argv = Some(env_stub(&out));
    client.coordinator_policy(&first.id, Some(settings)).unwrap();
    client.destroy(&first.id).unwrap();
    let coordinator = client.run(spec).unwrap().task.id;
    eventually(|| out.exists());
    let env = std::fs::read_to_string(&out).unwrap();
    let mut lines = env.lines();
    let (url, token) = (lines.next().unwrap().to_owned(), lines.next().unwrap().to_owned());
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert_eq!(token.len(), 64);

    // Without the token, or with another, nothing is learned.
    let tasks = json!({ "operation": "tasks" });
    assert_eq!(code(&post(&url, None, &tasks)), (401, "deck_control_unauthorized"));
    assert_eq!(code(&post(&url, Some(&"0".repeat(64)), &tasks)), (401, "deck_control_unauthorized"));
    let t = Some(token.as_str());
    let (status, listed) = post(&url, t, &tasks);
    assert_eq!((status, listed["truncated"].clone()), (200, json!(false)));
    let ids = |listed: &Value| -> Vec<String> {
        listed["tasks"].as_array().unwrap().iter().map(|task| task["id"].as_str().unwrap().to_owned()).collect()
    };
    assert_eq!(ids(&listed), std::slice::from_ref(&first.id), "the destroyed coordinator, never the caller");
    // Its project set, named, before it has created anything (contract 1.5.0).
    assert_eq!(listed["projects"], json!([{ "id": project.id, "name": project.name }]));

    // A user's own task in the project is readable but not drivable.
    let mine = client
        .create(Spec {
            argv: agent.clone(),
            cwd: repo.clone(),
            project: Some(project.id.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(post(&url, t, &json!({ "operation": "task", "id": mine.id })).0, 200);
    let prompt_mine = json!({ "operation": "prompt", "id": mine.id, "text": "hi" });
    assert_eq!(code(&post(&url, t, &prompt_mine)), (403, "deck_control_scope"));
    let prompt_self = json!({ "operation": "prompt", "id": coordinator, "text": "hi" });
    assert_eq!(code(&post(&url, t, &prompt_self)), (403, "deck_control_refused"));
    let wait_self = json!({ "operation": "wait", "id": coordinator, "wait_ms": 10 });
    assert_eq!(code(&post(&url, t, &wait_self)), (403, "deck_control_refused"));
    let elsewhere =
        json!({ "operation": "create", "project": "git:0000000000000000000000000000000000000000", "brief": "x" });
    let refused = post(&url, t, &elsewhere);
    assert_eq!(code(&refused), (403, "deck_control_scope"));
    let message = refused.1["error"]["message"].as_str().unwrap();
    assert!(message.ends_with(&format!("its projects are {}", project.id)), "{message}");
    let missing = json!({ "operation": "task", "id": "nosuchtask" });
    assert_eq!(code(&post(&url, t, &missing)), (404, "deck_control_unknown"));
    let long = json!({ "operation": "wait", "id": mine.id, "wait_ms": 300001 });
    assert_eq!(code(&post(&url, t, &long)), (400, "deck_control_bad_request"));

    // Create: the project's agent command, provenance, the brief as turn 1.
    let brief = "Fix the flaky login test\n\nIt fails one run in ten.";
    let create = json!({ "operation": "create", "project": project.id, "brief": brief, "budget": { "tokens": 1000 } });
    let (status, created) = post(&url, t, &create);
    assert_eq!(status, 200, "{created}");
    let worker = created["id"].as_str().unwrap().to_owned();
    let record = client.task(&worker).unwrap();
    assert_eq!(record.spec.argv, agent, "the project's agent command, never the coordinator's");
    assert_eq!(record.spec.created_by.as_deref(), Some(coordinator.as_str()));
    let (status, waited) = post(&url, t, &json!({ "operation": "wait", "id": worker, "wait_ms": 20000 }));
    assert_eq!(status, 200);
    assert_eq!(waited["timed_out"], false, "{waited}");
    // The fake asks permission, and only a window may answer it.
    assert_eq!(waited["task"]["turn"]["state"], "waiting", "{waited}");
    assert_eq!(waited["task"]["attention"], "question");
    assert_eq!(waited["task"]["title"], "Fix the flaky login test");
    assert_eq!(waited["task"]["created_by"], coordinator.as_str());
    let prompts = client.prompts(&worker).unwrap();
    assert_eq!((prompts[0].turn, prompts[0].from.as_deref()), (1, Some(coordinator.as_str())));
    assert_eq!(prompts[0].text, brief);
    let busy = json!({ "operation": "prompt", "id": worker, "text": "and the docs" });
    assert_eq!(code(&post(&url, t, &busy)), (409, "deck_control_busy"));
    let (status, timed) = post(&url, t, &json!({ "operation": "wait", "id": worker, "after_turn": 1, "wait_ms": 300 }));
    assert_eq!((status, timed["timed_out"].clone()), (200, json!(true)), "an open question does not wake it again");
    let (_, listed) = post(&url, t, &tasks);
    let listed = ids(&listed);
    assert_eq!(listed[..2], [worker.clone(), mine.id.clone()], "most recently created first");
    assert!(!listed.contains(&coordinator));

    // At the concurrency cap: refused, announced, raised by the user, then allowed.
    let mut notices = client.subscribe().unwrap();
    let mut settings = client.coordinator_policy(&coordinator, None).unwrap();
    settings.policy.max_concurrent = 1;
    client.coordinator_policy(&coordinator, Some(settings.clone())).unwrap();
    // No project named: the coordinator's only one (contract 1.5.0).
    let second = json!({ "operation": "create", "brief": "Second" });
    let refused = post(&url, t, &second);
    assert_eq!(code(&refused), (403, "deck_control_cap"));
    assert_eq!(refused.1["retryable"], true);
    let mut announced = None;
    eventually(|| {
        while let Ok(notice) = notices.try_recv() {
            announced = announced.take().or(notice.cap);
        }
        announced.is_some()
    });
    let cap = announced.unwrap();
    assert_eq!((cap.coordinator.as_str(), cap.cap.as_str(), cap.limit), (coordinator.as_str(), "max_concurrent", 1));
    settings.policy.max_concurrent = 4;
    settings.policy.creates_per_hour = 2;
    client.coordinator_policy(&coordinator, Some(settings)).unwrap();
    // Under the caps, but the first worker still runs in the project's one
    // local checkout (deck#200): retryable, and no Limit to raise.
    let shared = post(&url, t, &second);
    assert_eq!(code(&shared), (403, "deck_control_cap"));
    assert!(shared.1["error"]["message"].as_str().is_some_and(|m| m.contains("one runs at a time")), "{}", shared.1);
    // Stop answers the confirmed state, and makes room in the checkout.
    let (status, stopped) = post(&url, t, &json!({ "operation": "stop", "id": worker }));
    assert_eq!((status, stopped["state"].clone()), (200, json!("stopped")));
    let (status, again) = post(&url, t, &second);
    assert_eq!(status, 200, "{again}");
    let defaulted = client.task(again["id"].as_str().unwrap()).unwrap();
    assert_eq!(defaulted.spec.project.as_deref(), Some(project.id.as_str()));
    // The rolling-hour rate: two created, cap 2.
    let third = json!({ "operation": "create", "project": project.id, "brief": "Third" });
    assert_eq!(code(&post(&url, t, &third)), (403, "deck_control_cap"));

    // No refusal made an approval.
    let prompt_stopped = json!({ "operation": "prompt", "id": worker, "text": "more" });
    assert_eq!(code(&post(&url, t, &prompt_stopped)), (403, "deck_control_refused"));
    assert!(client.approvals(&worker).unwrap().is_empty());
    assert!(client.approvals(&coordinator).unwrap().is_empty());

    // The coordinator's record audits allowed and refused operations, with
    // no body: the brief never appears in it.
    let rows = audit(&daemon.data, &coordinator);
    let has = |op: &str, outcome: &str| rows.iter().any(|row| row["op"] == op && row["outcome"] == outcome);
    assert!(has("create", "ok") && has("create", "deck_control_cap") && has("prompt", "deck_control_busy"), "{rows:?}");
    assert!(has("tasks", "ok") && has("stop", "ok") && has("create", "deck_control_scope"), "{rows:?}");
    let log = std::fs::read_to_string(daemon.data.join("tasks").join(&coordinator).join("events.jsonl")).unwrap();
    assert!(!log.contains("flaky") && !log.contains(&token), "no prompt or token in the audit");
    let record = client.task(&worker).unwrap();
    assert!(record.turn.is_some_and(|turn| turn.number == 1 && turn.state != Phase::Working));

    // A coordinator whose policy is gone is refused, not unbounded.
    let policy = daemon.data.join("tasks").join(&coordinator).join("coordinator.json");
    std::fs::rename(&policy, policy.with_extension("moved")).unwrap();
    assert_eq!(code(&post(&url, t, &tasks)), (403, "deck_control_refused"));

    for id in [coordinator.as_str(), mine.id.as_str(), again["id"].as_str().unwrap()] {
        let _ = client.stop(id);
    }
}

#[test]
fn a_local_git_coordinator_runs_workers_in_separate_registered_worktrees() {
    let (daemon, client) = daemon("worktrees");
    let repo = daemon.data.join("repo");
    git_repo(&repo);
    let repo = repo.canonicalize().unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim_end().to_owned()
    };
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("README.md"), "dirty project files\n").unwrap();
    std::fs::write(repo.join("private"), "untracked project files\n").unwrap();
    let fake = deck_runtime::daemon_client::sibling_binary("deck-fake-agent").unwrap();
    let project = client.open_project(&repo, vec![fake.display().to_string()]).unwrap();
    let spec = Spec {
        cwd: repo.clone(),
        project: Some(project.id.clone()),
        kind: TaskKind::Coordinator,
        ..Default::default()
    };
    let first = client.create(spec.clone()).unwrap();
    let out = daemon.data.join("worktrees-control-env");
    let mut settings = client.coordinator_policy(&first.id, None).unwrap();
    settings.coordinator_argv = Some(env_stub(&out));
    client.coordinator_policy(&first.id, Some(settings)).unwrap();
    client.destroy(&first.id).unwrap();
    client.run(spec).unwrap();
    eventually(|| out.exists());
    let env = std::fs::read_to_string(&out).unwrap();
    let mut lines = env.lines();
    let url = lines.next().unwrap();
    let token = lines.next().unwrap();
    let create = |brief: &str| {
        let (status, reply) = post(url, Some(token), &json!({ "operation": "create", "brief": brief }));
        assert_eq!(status, 200, "{reply}");
        reply["id"].as_str().unwrap().to_owned()
    };
    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| create("first worker"));
        let second = create("second worker");
        (first.join().unwrap(), second)
    });
    assert_ne!(a, b);
    for id in [&a, &b] {
        eventually(|| client.task(id).is_ok_and(|task| task.state == deck_runtime::State::Running));
        let task = client.task(id).unwrap();
        let workspace = task.workspace.unwrap();
        assert!(workspace.ready);
        assert_eq!(workspace.start, base);
        assert_eq!(workspace.source, repo);
        let worktree = task.spec.cwd;
        assert_eq!(worktree, daemon.data.canonicalize().unwrap().join("tasks").join(id).join("worktree"));
        assert_eq!(git(&worktree, &["branch", "--show-current"]), format!("deck/{id}"));
        assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), base);
        assert_eq!(std::fs::read_to_string(worktree.join("README.md")).unwrap(), "one\n");
        assert!(!worktree.join("private").exists());
        let reopened = client.open_project(&worktree, vec![]).unwrap();
        assert_eq!(reopened.checkouts, vec![repo.clone()]);
    }
    let worktree = |id: &str| client.task(id).unwrap().spec.cwd;
    let commit = |cwd: &Path, content: &str| {
        std::fs::write(cwd.join("README.md"), content).unwrap();
        git(cwd, &["add", "--", "README.md"]);
        git(cwd, &["commit", "-m", "Contributor update"]);
        git(cwd, &["rev-parse", "HEAD"])
    };
    let a_pin = commit(&worktree(&a), "first contributor\n");
    let request = json!({
        "operation": "integrate", "destination": { "kind": "local_branch", "branch": "main" },
        "contributors": [a, b], "brief": "Keep both README edits and run the combined checks."
    });
    let (status, created) = post(url, Some(token), &request);
    assert_eq!(status, 200, "{created}");
    let integration = created["id"].as_str().unwrap();
    let wait = json!({ "operation": "wait", "id": integration, "wait_ms": 10000 });
    let (status, result) = post(url, Some(token), &wait);
    assert_eq!(status, 200, "{result}");
    let plan = &result["task"]["integration"];
    assert_eq!(plan["phase"], "waiting", "{result}");
    assert!(plan["reason"].as_str().unwrap().contains("no committed work"), "{result}");
    assert_eq!(plan["base_commit"], base);
    assert_eq!(plan["contributors"][0], json!({"id": a, "commit": a_pin}));
    assert_eq!(plan["contributors"][1], json!({"id": b}));
    assert!(!worktree(integration).exists());

    let a_later = commit(&worktree(&a), "later contributor work\n");
    let b_pin = commit(&worktree(&b), "second contributor\n");
    assert_eq!(post(url, Some(token), &request), (200, created.clone()));
    eventually(|| client.prompts(integration).is_ok_and(|prompts| !prompts.is_empty()));
    let detail = || {
        let (status, result) = post(url, Some(token), &json!({ "operation": "task", "id": integration }));
        assert_eq!(status, 200, "{result}");
        result
    };
    let result = detail();
    let plan = &result["task"]["integration"];
    assert_eq!(plan["phase"], "ready");
    assert_eq!(plan["contributors"], json!([{"id":a,"commit":a_pin},{"id":b,"commit":b_pin}]));
    assert_eq!(plan["branch"], format!("deck/{integration}"));
    assert_eq!(result["task"]["worktree"]["target_ref"], "refs/heads/main");
    assert!(result["task"]["worktree"].get("review_commit").is_none());
    let brief = &client.prompts(integration).unwrap()[0].text;
    assert!(brief.find(&a_pin).unwrap() < brief.find(&b_pin).unwrap());
    assert!(brief.contains("developer merge_start"));

    let integrate_at = worktree(integration);
    assert_eq!(git(&integrate_at, &["rev-parse", "HEAD"]), base);
    git(&integrate_at, &["merge", "--no-ff", "--no-edit", &a_pin]);
    assert!(detail()["task"]["worktree"].get("review_commit").is_none());
    let conflict = Command::new("git")
        .current_dir(&integrate_at)
        .args(["merge", "--no-ff", "--no-edit", &b_pin])
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(1));
    assert!(git(&integrate_at, &["status", "--porcelain"]).contains("UU README.md"));
    let reviewed = commit(&integrate_at, "first contributor\nsecond contributor\n");
    assert_eq!(
        git(&integrate_at, &["show", &format!("{reviewed}:README.md")]),
        "first contributor\nsecond contributor"
    );
    for pin in [&base, &a_pin, &b_pin] {
        git(&integrate_at, &["merge-base", "--is-ancestor", pin, &reviewed]);
    }
    assert_eq!(detail()["task"]["worktree"]["review_commit"], reviewed);
    assert_eq!(git(&worktree(&a), &["rev-parse", "HEAD"]), a_later);
    assert_eq!(git(&worktree(&b), &["rev-parse", "HEAD"]), b_pin);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), base);

    let worktree = client.task(&a).unwrap().spec.cwd;
    assert_eq!(
        client.run(Spec { cwd: worktree.clone(), ..Default::default() }).unwrap().task.id,
        a,
        "opening an existing worktree should select its task",
    );
    client.stop(&a).unwrap();
    std::fs::write(worktree.join("uncommitted"), "remember me").unwrap();
    let moved = worktree.with_extension("moved");
    std::fs::rename(&worktree, &moved).unwrap();
    assert!(client.resume(&a).is_err());
    assert!(!worktree.exists());
    assert_eq!(std::fs::read_to_string(moved.join("uncommitted")).unwrap(), "remember me");
    assert_eq!(git(&repo, &["rev-parse", "main"]), base);
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "dirty project files\n");
}
