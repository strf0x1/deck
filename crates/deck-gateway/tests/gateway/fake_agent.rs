//! The fake agent's publication turn against the real gateway binary and a
//! stub GitHub: own-branch admission for the publish and the draft pull
//! request, a card for marking it ready, and the gateway's reply or refusal in
//! the transcript as each step's tool result (ADR 0008 §9).

use super::*;
use serde_json::json;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const BRANCH: &str = "deck/t-1";

/// The fake agent is a sibling of this test's binary in the workspace target
/// directory; `cargo test` builds every workspace binary before running any
/// test. A package-local `cargo test -p deck-gateway` does not, so the
/// message says what to build.
fn fake_agent() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target = exe.parent().and_then(Path::parent).unwrap();
    let path = target.join(format!("deck-fake-agent{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "{} is missing; run `cargo build -p deck-acp --bin deck-fake-agent` or the workspace's `cargo test`",
        path.display()
    );
    path
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["-c", "user.name=deck", "-c", "user.email=deck@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} in {}", cwd.display());
}

fn git_text(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The checkout the guest would have: on the task branch, with history.
fn checkout(name: &str) -> PathBuf {
    let work = scratch(name).join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "--quiet"]);
    git(&work, &["checkout", "--quiet", "-b", BRANCH]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "base.txt"]);
    git(&work, &["commit", "--quiet", "-m", "base"]);
    work
}

/// The tip the bare repository has, as `refs` would report it.
fn remote_tip(remote: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(remote)
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{BRANCH}")])
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A pull request summary the gateway's client can read.
fn pull_request(draft: bool) -> String {
    json!({
        "number": 7, "title": "Ship it", "state": "open", "draft": draft, "merged": false,
        "head": { "ref": BRANCH, "sha": "c".repeat(40), "repo": { "full_name": "owner/repo" } },
        "base": { "ref": "main" }, "mergeable": null,
        "html_url": "https://github.com/owner/repo/pull/7", "user": { "login": "x" },
    })
    .to_string()
}

/// The approval queue's own-branch admission, recording every request so the
/// case can check what the gateway asked and what it reported.
fn recording_queue(name: &str) -> (PathBuf, Arc<Mutex<Vec<Value>>>) {
    let dir = scratch(name);
    let socket = dir.join("deckd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            if request["op"] == "publication" {
                let _ = stream.write_all(b"{\"ok\":null}\n");
                log.lock().unwrap().push(request);
                continue;
            }
            let coverage = &request["coverage"];
            let own = coverage["own_branch"].as_bool().unwrap_or(false);
            let public = coverage["public"].as_bool();
            let admitted = own && public == Some(false);
            let state = match admitted {
                true => json!({ "allowed": "default" }),
                false => json!("pending"),
            };
            let reply = json!({
                "ok": { "id": "g1", "gateway": "github", "operation": request["operation"],
                        "destination": request["destination"], "state": state }
            });
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
            log.lock().unwrap().push(request);
        }
    });
    (socket, seen)
}

/// A byte relay from the agent's loopback HTTP to the gateway's Unix socket.
/// The protocol is the gateway's; nothing here parses it.
struct Bridge {
    url: String,
}

impl Bridge {
    fn new(target: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let target = target.clone();
                std::thread::spawn(move || {
                    let Ok(unix) = UnixStream::connect(&target) else { return };
                    let (mut tcp, mut unix_read) = (stream, unix.try_clone().unwrap());
                    let mut tcp_read = tcp.try_clone().unwrap();
                    let mut unix_write = unix;
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut tcp_read, &mut unix_write);
                    });
                    let _ = std::io::copy(&mut unix_read, &mut tcp);
                    // The gateway sent its framed reply and closed; tell the
                    // agent so a client that waits for end-of-file proceeds.
                    let _ = tcp.shutdown(std::net::Shutdown::Both);
                });
            }
        });
        Self { url }
    }
}

/// The ACP client side, raw: the test asserts on the transcript, including
/// the tool result content the typed client does not expose.
struct Raw {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}

impl Raw {
    fn start(agent: &Path, cwd: &Path, url: &str) -> Self {
        let mut child = Command::new(agent)
            .env("DECK_GITHUB_URL", url)
            .env("DECK_FAKE_DELAY_MS", "0")
            .env_remove("DECK_BOUNDARY_URL")
            .env_remove("DECK_FAKE_NAMES")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output, next: 0 }
    }

    fn send(&mut self, value: Value) {
        writeln!(self.input, "{value}").unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Send a request; return the notifications before its reply, and the reply.
    fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        self.next += 1;
        let id = self.next;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut before = vec![];
        loop {
            let value = self.read();
            if value["id"] == json!(id) && value.get("method").is_none() {
                return (before, value);
            }
            before.push(value);
        }
    }

    fn finish(mut self) {
        drop(self.input);
        assert!(self.child.wait().unwrap().success());
    }
}

/// One prompt, one publication turn; the updates before the reply.
fn run_turn(agent: &Path, cwd: &Path, url: &str, prompt: &str) -> Vec<Value> {
    let mut raw = Raw::start(agent, cwd, url);
    let init = json!({
        "protocolVersion": 1,
        "clientCapabilities": { "_meta": { "com.sprawlcommons/construct": { "events": true, "turn": true } } },
    });
    let (_, reply) = raw.call("initialize", init);
    assert_eq!(reply["result"]["agentCapabilities"]["_meta"]["com.sprawlcommons/deck"]["github_operations"], json!(1));
    let (_, new) = raw.call("session/new", json!({}));
    let session = new["result"]["sessionId"].as_str().unwrap().to_string();
    let (updates, reply) =
        raw.call("session/prompt", json!({ "sessionId": session, "prompt": [{ "type": "text", "text": prompt }] }));
    assert_eq!(reply["result"]["stopReason"], "end_turn");
    raw.finish();
    updates
}

/// The text of each tool update, in the order the agent sent them.
fn tool_results(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|update| update["params"]["update"]["sessionUpdate"] == "tool_call_update")
        .filter_map(|update| update["params"]["update"]["content"][0]["content"]["text"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn the_fake_agent_admits_the_publish_and_the_draft_pr_and_asks_to_mark_ready() {
    let (dir, remote) = bare("fa-r");
    let work = checkout("fa");
    let remote_for_api = remote.clone();
    let (api, requests, _) = api(move |request| match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/repos/owner/repo") => r#"{"private":true}"#.into(),
        ("GET", target) if target.ends_with("/git/matching-refs/heads/") => match remote_tip(&remote_for_api) {
            Some(sha) => format!(r#"[{{"ref":"refs/heads/{BRANCH}","object":{{"sha":"{sha}"}}}}]"#),
            None => "[]".into(),
        },
        ("POST", target) if target.ends_with("/pulls") => {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            json!({
                "number": 7, "title": body["title"], "state": "open", "draft": body["draft"], "merged": false,
                "head": { "ref": BRANCH, "sha": "c".repeat(40), "repo": { "full_name": "owner/repo" } },
                "base": { "ref": "main" }, "mergeable": null,
                "html_url": "https://github.com/owner/repo/pull/7", "user": { "login": "x" },
            })
            .to_string()
        }
        ("GET", target) if target.ends_with("/pulls/7") => pull_request(true),
        ("PATCH", target) if target.ends_with("/pulls/7") => pull_request(false),
        ("GET", target) if target.contains("/check-runs") => {
            r#"{"total_count":1,"check_runs":[{"name":"ci","status":"completed","conclusion":"success"}]}"#.into()
        }
        _ => "{}".into(),
    });
    let (queue, seen) = recording_queue("fa-q");
    let gateway = with_github("fa-g", &api, &url(&remote), Some((&queue, "t-1")), Some("token"));
    let bridge = Bridge::new(gateway.ready.github.clone().unwrap());
    let agent = fake_agent();

    let updates = run_turn(&agent, &work, &bridge.url, "Ship it");
    let results = tool_results(&updates);
    assert!(results.iter().any(|text| text.contains(r#""task_branch":"deck/t-1""#)), "{updates:?}");
    assert!(
        results.iter().any(|text| text.contains(r#""branch":"deck/t-1""#) && text.contains(r#""commit""#)),
        "{results:?}"
    );
    assert!(results.iter().any(|text| text.contains(r#""number":7"#)), "{results:?}");
    assert!(results.iter().any(|text| text.contains(r#""checks""#)), "{results:?}");
    assert!(
        results.iter().any(|text| text.contains("needs the user's approval") && text.contains("g1")),
        "the pending ready card is the update's tool result: {results:?}"
    );

    let seen = seen.lock().unwrap();
    let approvals: Vec<&Value> = seen.iter().filter(|request| request["op"] == "approval").collect();
    assert_eq!(approvals.len(), 3, "{approvals:?}");
    let publish = approvals.iter().find(|request| request["operation"] == "publish").expect("a publish ask");
    assert_eq!(publish["destination"], "owner/repo#deck/t-1");
    assert_eq!(publish["coverage"], json!({ "own_branch": true, "public": false }));
    assert_eq!(publish["details"]["summary"]["subject"], "deck-fake-agent: Ship it");
    assert_eq!(publish["details"]["summary"]["count"], 2, "the base and the turn's commit");
    let create = approvals.iter().find(|request| request["operation"] == "create_pr").expect("a create ask");
    assert_eq!(create["destination"], "owner/repo#deck/t-1");
    assert_eq!(create["coverage"], json!({ "own_branch": true, "public": false }));
    let update = approvals.iter().find(|request| request["operation"] == "update_pr").expect("a ready ask");
    assert_eq!(update["destination"], "owner/repo#pr7");
    assert_eq!(update["reason"], "update pull request 7");
    assert!(
        update["details"]["identity"].as_str().is_some_and(|identity| identity.starts_with("sha256:")),
        "the reviewed write is identified: {update}"
    );
    assert!(update.get("coverage").is_none(), "ready-for-review is never admitted by default: {update}");

    // The publication reports record what landed, which is what the task
    // record (#82) folds into the latest publication.
    let reports: Vec<&Value> = seen.iter().filter(|request| request["op"] == "publication").collect();
    assert_eq!(reports.len(), 2, "{reports:?}");
    let branch = reports.iter().find(|report| report["publication"]["kind"] == "branch").expect("a branch");
    assert_eq!(branch["task"], "t-1");
    assert_eq!(branch["publication"]["branch"], BRANCH);
    assert_eq!(Some(branch["publication"]["commit"].as_str().unwrap().to_string()), remote_tip(&remote));
    let pull = reports.iter().find(|report| report["publication"]["kind"] == "pull_request").expect("a pull request");
    assert_eq!(pull["publication"]["number"], 7);
    assert_eq!(pull["publication"]["title"], "Ship it");
    assert_eq!(pull["publication"]["draft"], true);

    // GitHub saw one draft pull request and no patch: the ready update was
    // stopped at the card.
    let requests = requests.lock().unwrap();
    let posts: Vec<_> =
        requests.iter().filter(|request| request.method == "POST" && request.target.ends_with("/pulls")).collect();
    assert_eq!(posts.len(), 1, "one pull request");
    assert!(std::str::from_utf8(&posts[0].body).unwrap().contains(r#""draft":true"#));
    assert!(requests.iter().all(|request| request.method != "PATCH"), "the ready update never reached GitHub");
    // The stub's handlers take these locks on every request; a second turn
    // would wait for a reply they cannot give while the test holds them.
    drop(requests);
    drop(seen);
    let _ = dir;

    // A second turn publishes the next commit incrementally: the gateway
    // names the tip it fast-forwarded and the branch gains one commit.
    let updates = run_turn(&agent, &work, &bridge.url, "Ship again");
    let results = tool_results(&updates);
    let publish = results.iter().find(|text| text.contains(r#""previous""#)).expect("a publish result");
    assert!(publish.contains(r#""created":false"#), "{publish}");
    assert!(publish.contains(r#""previous":""#), "{publish}");
    let count = git_text(&work, &["rev-list", "--count", &format!("refs/heads/{BRANCH}")]);
    assert_eq!(count, "3", "base, first turn, second turn");
    let remote_count = Command::new("git")
        .arg("--git-dir")
        .arg(&remote)
        .args(["rev-list", "--count", &format!("refs/heads/{BRANCH}")])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&remote_count.stdout).trim(), "3");
}
