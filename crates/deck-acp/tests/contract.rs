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
