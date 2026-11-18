//! The shared ACP extension contract (`CONTRACT.lock`): the names Deck sends
//! and the forms it accepts must match the vendored matrix. A mismatch means
//! the contract moved without Deck or Deck moved without the contract.

#![expect(clippy::unwrap_used, reason = "a fixture and a pin that do not read are the failure")]

use deck_acp::names::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn lock() -> HashMap<String, String> {
    let text = std::fs::read_to_string(root().join("CONTRACT.lock")).unwrap();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.trim().to_string(), v.trim().trim_matches('"').to_string())
        })
        .collect()
}

fn vendored() -> PathBuf {
    let l = lock();
    root().join(&l["vendored"])
}

fn matrix() -> Value {
    serde_json::from_str(&std::fs::read_to_string(vendored().join("matrix.json")).unwrap()).unwrap()
}

fn list(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect()
}

#[test]
fn the_vendored_copy_is_the_pinned_version() {
    let version = std::fs::read_to_string(vendored().join("VERSION")).unwrap();
    assert_eq!(version.trim(), lock()["version"]);
    assert_eq!(matrix()["contract"], json!(lock()["version"]));
    assert_eq!(matrix()["acp"], json!("1"));
}

#[test]
fn the_names_are_the_contracts_names() {
    let names = &matrix()["names"];
    assert_eq!(names["construct_meta"], CONSTRUCT_META);
    assert_eq!(names["construct_meta_legacy"], LEGACY_CONSTRUCT_META);
    assert_eq!(names["deck_meta"], DECK_META);
    assert_eq!(names["deck_meta_legacy"], LEGACY_DECK_META);
    assert_eq!(names["turn_method"], TURN_METHOD);
    assert_eq!(names["turn_method_legacy"], LEGACY_TURN_METHOD);
    assert_eq!(names["boundary_method"], BOUNDARY_METHOD);
    assert_eq!(names["boundary_method_legacy"], LEGACY_BOUNDARY_METHOD);
}

#[test]
fn the_forms_deck_accepts_follow_the_contract() {
    let accept = &matrix()["accepts"];
    assert_eq!(list(&accept["boundary_methods"]), [BOUNDARY_METHOD, LEGACY_BOUNDARY_METHOD]);
    assert_eq!(list(&accept["load_snapshot_keys"]), [CONSTRUCT_META, LEGACY_CONSTRUCT_META]);
    // Deck asks for the current turn method with this capability (conn.rs).
    let m = matrix();
    let current = m["negotiation"].as_array().unwrap().iter().find(|c| c["case"] == "current").unwrap();
    assert_eq!(current["client_meta"][CONSTRUCT_META]["turn"], json!(true));
    assert_eq!(current["expect"]["turn_method"], TURN_METHOD);
}

#[test]
fn the_github_capability_is_the_contracts() {
    let m = matrix();
    let capability = &m["github"]["capability"];
    assert_eq!(capability["meta_key"], DECK_META);
    assert_eq!(capability["field"], GITHUB_OPERATIONS);
    assert_eq!(capability["version"], json!(GITHUB_OPERATIONS_VERSION));
    assert_eq!(capability["legacy_key"], json!(false), "Deck reads it under the current key only (conn.rs)");
}

#[test]
fn idempotent_prompts_are_the_contracts() {
    let m = matrix();
    let idem = &m["idempotent_prompts"];
    assert_eq!(idem["capability_key"], CONSTRUCT_META, "read under the current key only (conn.rs)");
    assert_eq!(idem["meta_key"], CONSTRUCT_META);
    assert_eq!(idem["capability"], IDEMPOTENT_PROMPTS);
    assert_eq!(idem["field"], IDEMPOTENCY_KEY);
    assert_eq!(idem["conflict_error"]["reason"], IDEMPOTENCY_CONFLICT);
    // Keys Deck mints fit the contract's bounds.
    let key = deck_acp::key::new_key();
    let max = idem["max_length"].as_u64().unwrap() as usize;
    assert!((idem["min_length"].as_u64().unwrap() as usize..=max).contains(&key.chars().count()), "{key}");
}

#[test]
fn coordinator_control_is_the_contracts() {
    let m = matrix();
    let control = &m["control"];
    assert_eq!(control["since"], "1.4.0");
    assert_eq!(control["method"], "POST");
    assert_eq!(control["path"], CONTROL_PATH);
    assert_eq!(control["environment"]["url"], CONTROL_URL_ENV);
    assert_eq!(control["environment"]["token"], CONTROL_TOKEN_ENV);
    assert_eq!(control["capability"], Value::Null, "discovered from the environment, not initialize");
    let operations: Vec<&str> =
        control["operations"].as_array().unwrap().iter().map(|op| op["operation"].as_str().unwrap()).collect();
    assert_eq!(operations, ["tasks", "task", "create", "prompt", "wait", "stop"]);
    let codes: Vec<&str> =
        control["refusals"].as_array().unwrap().iter().map(|row| row["code"].as_str().unwrap()).collect();
    assert_eq!(
        codes,
        [
            "deck_control_bad_request",
            "deck_control_unauthorized",
            "deck_control_scope",
            "deck_control_cap",
            "deck_control_refused",
            "deck_control_unknown",
            "deck_control_busy",
            "deck_control_unavailable",
        ]
    );
    // Every exchange names a schema'd request or a refusal the table lists.
    let schemas = validators();
    let exchanges = control["exchanges"].as_array().unwrap();
    assert!(exchanges.len() >= 45, "the 1.4.0 exchange table");
    for exchange in exchanges {
        let expect = &exchange["expect"];
        let name = if expect["status"] == 200 { None } else { Some("control-error.schema.json") };
        if let Some(name) = name {
            schemas[name].validate(&expect["body"]).unwrap();
            assert!(codes.contains(&expect["code"].as_str().unwrap()), "{}", exchange["case"]);
        }
    }
}

fn validators() -> HashMap<String, jsonschema::Validator> {
    let schemas: Vec<_> = std::fs::read_dir(vendored().join("schema"))
        .unwrap()
        .map(|p| {
            let path = p.unwrap().path();
            let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            (path.file_name().unwrap().to_str().unwrap().to_string(), schema)
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

#[test]
fn shared_payload_fixtures_validate_offline() {
    let schemas = validators();
    let fixtures: Value =
        serde_json::from_str(&std::fs::read_to_string(vendored().join("payloads.json")).unwrap()).unwrap();
    assert_eq!(fixtures["contract"], lock()["version"]);
    let mut covered = std::collections::HashSet::new();
    for case in fixtures["cases"].as_array().unwrap() {
        let name = case["schema"].as_str().unwrap();
        covered.insert(name);
        assert_eq!(schemas[name].is_valid(&case["payload"]), case["valid"].as_bool().unwrap(), "{}", case["case"]);
    }
    assert_eq!(covered.len(), schemas.len());
}

#[test]
fn actual_client_capabilities_and_boundary_notifications_conform() {
    use deck_acp::conn::Conn;
    let schemas = validators();
    for key in [DECK_META, LEGACY_DECK_META] {
        let (mut conn, line) = Conn::start(PathBuf::from("/workspace"));
        let init: Value = serde_json::from_str(&line).unwrap();
        schemas["client-capabilities.schema.json"].validate(&init["params"]["clientCapabilities"]["_meta"]).unwrap();
        let response = json!({ "jsonrpc": "2.0", "id": init["id"], "result": {
            "agentInfo": { "name": "construct" }, "agentCapabilities": { "_meta": { key: { "boundary": true } } }
        } });
        conn.read(&response.to_string());
        conn.read(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#);
        let line = conn
            .boundary_notice(&json!({
                "approval_id": "a1", "destination": "example.com:443", "outcome": "allowed", "scope": "task"
            }))
            .unwrap();
        let notice: Value = serde_json::from_str(&line).unwrap();
        schemas["boundary-notice.schema.json"].validate(&notice["params"]).unwrap();
        assert_eq!(notice["method"], if key == DECK_META { BOUNDARY_METHOD } else { LEGACY_BOUNDARY_METHOD });
        assert!(notice.get("id").is_none(), "boundary decisions are notifications, never requests");
    }
}
