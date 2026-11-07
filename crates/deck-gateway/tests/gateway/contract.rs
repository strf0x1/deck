//! The GitHub gateway against the shared contract it serves (contract 1.2.0
//! §11, pinned by `CONTRACT.lock`): its operation table, limits and refusal
//! codes are the matrix's, its request parser agrees with every request
//! fixture, the publish framing cases get the conclusion the matrix states,
//! and every reply a real gateway sends validates against the vendored schema.

use super::*;
use deck_gateway::github::policy::{self, Ask, Operation};
use serde_json::json;
use std::collections::{HashMap, HashSet};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn vendored() -> PathBuf {
    let lock = std::fs::read_to_string(root().join("CONTRACT.lock")).unwrap();
    let line = lock.lines().find(|line| line.starts_with("vendored")).unwrap();
    root().join(line.split_once('=').unwrap().1.trim().trim_matches('"'))
}

fn read(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(vendored().join(name)).unwrap()).unwrap()
}

fn github() -> Value {
    read("matrix.json")["github"].clone()
}

/// Every vendored schema, compiled with the others as its registry.
fn validators() -> HashMap<String, jsonschema::Validator> {
    let schemas: Vec<(String, Value)> = std::fs::read_dir(vendored().join("schema"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            (name, serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap())
        })
        .collect();
    let mut registry = jsonschema::Registry::new();
    for (_, schema) in &schemas {
        registry = registry.add(schema["$id"].as_str().unwrap(), schema.clone()).unwrap();
    }
    let registry = registry.prepare().unwrap();
    schemas
        .iter()
        .map(|(name, schema)| (name.clone(), jsonschema::options().with_registry(&registry).build(schema).unwrap()))
        .collect()
}

/// A raw reply split into its status and JSON body.
fn reply(raw: &str) -> (u16, Value) {
    let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
    let body = raw.split_once("\r\n\r\n").unwrap().1;
    (status, serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {raw}")))
}

/// Check one reply against the matrix: a success against `schema`, a refusal
/// against `github-error` with a status the refusal table allows.
fn conforms(schemas: &HashMap<String, jsonschema::Validator>, raw: &str, schema: &str) -> (u16, Value) {
    let (status, body) = reply(raw);
    let name = if status == 200 { schema } else { "github-error.schema.json" };
    if let Err(error) = schemas[name].validate(&body) {
        panic!("{name}: {error}\n{raw}");
    }
    if status != 200 {
        let table = github()["refusals"].clone();
        let code = &body["error"]["code"];
        let row = table.as_array().unwrap().iter().find(|row| &row["code"] == code).unwrap();
        assert!(row["status"].as_array().unwrap().contains(&json!(status)), "{code} is never {status}: {raw}");
        if !row["retryable"].is_null() {
            assert_eq!(body["retryable"], row["retryable"], "{raw}");
        }
    }
    (status, body)
}

fn post(socket: &Path, path: &str, body: &[u8]) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    stream.write_all(format!("POST {path} HTTP/1.1\r\ncontent-length: {}\r\n\r\n", body.len()).as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut bits, mut count) = (0u32, 0);
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        bits = (bits << 6) | ALPHABET.iter().position(|c| *c == byte).unwrap() as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out
}

#[test]
fn the_operation_table_limits_and_refusals_are_the_contracts() {
    let github = github();
    let table: Vec<(String, String, bool, bool)> = github["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| {
            let text = |key: &str| op[key].as_str().unwrap().to_string();
            (text("operation"), text("path"), op["writes"] == true, op["bundle"] == true)
        })
        .collect();
    let ours: Vec<_> = policy::operations()
        .map(|op| (op.name().to_string(), op.path().to_string(), op.asks(), op.carries_bundle()))
        .collect();
    assert_eq!(ours, table);
    let refused: Vec<&str> =
        github["refused_by_name"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert_eq!(Operation::refused(), refused.as_slice());
    let limits = &github["limits"];
    assert_eq!(limits["json_head_bytes"], json!(deck_gateway::github::MAX_ASK));
    assert_eq!(limits["bundle_bytes"], json!(deck_gateway::github::bundle::MAX_BUNDLE));
    assert_eq!(limits["pr_text_bytes"], json!(deck_gateway::github::client::MAX_TEXT));
    let publish = read("schema/github-publish-request.schema.json");
    assert_eq!(publish["properties"]["summary"]["properties"]["subject"]["maxLength"], json!(policy::MAX_SUBJECT));
}

#[test]
fn the_request_parser_agrees_with_every_request_fixture() {
    let paths: HashMap<String, Operation> = github()["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| {
            (
                op["request"].as_str().unwrap().to_string(),
                *policy::operations().find(|o| o.name() == op["operation"]).unwrap(),
            )
        })
        .collect();
    let mut covered = HashSet::new();
    for case in read("payloads.json")["cases"].as_array().unwrap() {
        let Some(operation) = paths.get(case["schema"].as_str().unwrap()) else { continue };
        covered.insert(operation.name());
        let parsed = Ask::parse(case["payload"].to_string().as_bytes());
        let accepted = matches!(&parsed, Ok((_, parsed)) if parsed == operation);
        // The schema checks a branch's alphabet; the parser leaves names to
        // the policy, which refuses them later. No fixture relies on that.
        assert_eq!(accepted, case["valid"] == true, "{}: {parsed:?}", case["case"]);
    }
    assert_eq!(covered.len(), 5, "every operation has request fixtures");
}

#[test]
fn publish_framing_cases_get_the_matrixs_conclusion() {
    let schemas = validators();
    let (_, remote) = bare("gh-contract-framing");
    let (api, _, _) = api(|_| r#"{"private":true}"#.into());
    let queue = agreement("gh-contract-framing-queue");
    let gateway = with_github("gh-contract-framing-gw", &api, &url(&remote), Some((&queue, "t-123")), Some("t"));
    let socket = gateway.ready.github.clone().unwrap();
    // In order: the full bundle creates the branch, so the incremental one's
    // prerequisite is on it and fast-forwards nothing.
    for case in github()["framing_cases"].as_array().unwrap() {
        let raw = post(&socket, "/operations/publish", &base64(case["body_base64"].as_str().unwrap()));
        let (status, body) = conforms(&schemas, &raw, "github-publish-response.schema.json");
        let expect = &case["expect"];
        if expect["valid"] == true {
            assert_eq!(status, 200, "{}: {raw}", case["case"]);
            assert_eq!(body["branch"], expect["branch"]);
            assert_eq!(body["commit"], expect["commit"]);
        } else {
            assert_eq!(json!(status), expect["status"], "{}: {raw}", case["case"]);
            assert_eq!(body["error"]["code"], expect["code"], "{}", case["case"]);
        }
    }
}

#[test]
fn every_reply_a_gateway_sends_is_the_contracts_shape() {
    let schemas = validators();
    let (_, remote) = bare("gh-contract-replies");
    let head = "a".repeat(40);
    let base = "b".repeat(40);
    let pull = format!(
        r#"{{"number":7,"title":"Add b","state":"open","draft":true,"merged":false,"mergeable":null,
            "head":{{"ref":"deck/t-1","sha":"{head}","repo":{{"full_name":"owner/repo"}}}},"base":{{"ref":"main"}},
            "html_url":"https://github.com/owner/repo/pull/7","user":{{"login":"x"}}}}"#
    );
    let (api, _, _) = api(move |request| match (request.method.as_str(), request.target.as_str()) {
        (_, t) if t.ends_with("/git/matching-refs/heads/") => {
            format!(r#"[{{"ref":"refs/heads/main","object":{{"sha":"{base}"}}}}]"#)
        }
        (_, t) if t.contains("/check-runs") => {
            r#"{"total_count":1,"check_runs":[{"name":"ci","status":"in_progress","conclusion":null}]}"#.into()
        }
        (_, t) if t.ends_with("/pulls/404") => r#"!404 {"message":"Not Found"}"#.into(),
        (_, t) if t.ends_with("/pulls") || t.ends_with("/pulls/7") => pull.clone(),
        _ => r#"{"private":true}"#.into(),
    });
    let queue = agreement("gh-contract-replies-queue");
    let gateway = with_github("gh-contract-replies-gw", &api, &url(&remote), Some((&queue, "t-1")), Some("t"));
    let socket = gateway.ready.github.clone().unwrap();
    let call = |path: &str, body: Value, schema: &str| {
        conforms(&schemas, &post(&socket, path, body.to_string().as_bytes()), schema)
    };

    // Before the first publish the guest still learns where it may publish.
    let (status, refs) = call("/operations/refs", json!({ "operation": "refs" }), "github-refs-response.schema.json");
    assert_eq!(status, 200);
    assert_eq!(refs["task_branch"], "deck/t-1");
    assert_eq!(refs["writable"], json!(["deck/t-1"]));
    assert_eq!(refs["base"], "main");
    let pr = json!({ "operation": "create_pr", "head": "deck/t-1", "base": "main", "title": "Add b" });
    assert_eq!(call("/operations/pull_request", pr, "github-pull-request.schema.json").0, 200);
    let update = json!({ "operation": "update_pr", "number": 7, "body": "" });
    assert_eq!(call("/operations/pull_request/update", update, "github-pull-request.schema.json").0, 200);
    let (status, review) = call(
        "/operations/pull_request/review",
        json!({ "operation": "review", "number": 7 }),
        "github-review-response.schema.json",
    );
    assert_eq!(status, 200);
    assert_eq!(review["merged"], false);

    // Refusals, each in the one shape, with the table's status.
    let refusals = [
        (
            "/operations/publish",
            json!({ "operation": "publish", "branch": "deck/t-1", "force": true }),
            "deck_bad_request",
        ),
        ("/operations/pull_request/update", json!({ "operation": "merge", "number": 7 }), "deck_not_permitted"),
        ("/operations/merge", json!({ "operation": "merge" }), "deck_not_permitted"),
        ("/repos/owner/repo/pulls", json!({}), "deck_not_permitted"),
        ("/operations/pull_request/review", json!({ "operation": "refs" }), "deck_bad_request"),
        ("/operations/pull_request/review", json!({ "operation": "review", "number": 404 }), "deck_github_not_found"),
        (
            "/operations/pull_request",
            json!({ "operation": "create_pr", "head": "main", "base": "main", "title": "t" }),
            "deck_github_refused",
        ),
    ];
    for (path, body, code) in refusals {
        let (status, reply) = call(path, body.clone(), "github-error.schema.json");
        assert_ne!(status, 200, "{path} {body}");
        assert_eq!(reply["error"]["code"], code, "{path} {body}: {reply}");
    }
    let raw = send(&socket, "GET /operations/refs HTTP/1.1\r\ncontent-length: 0\r\n\r\n");
    let (status, reply) = conforms(&schemas, &raw, "github-error.schema.json");
    assert_eq!((status, reply["error"]["code"].as_str()), (405, Some("deck_bad_request")));
}
