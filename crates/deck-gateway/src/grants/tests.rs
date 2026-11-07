//! Grant selection, policy replacement, expiry and the admission/register
//! race. The binary tests prove shutdown through real proxy connections.

use super::*;
use serde_json::json;

fn file(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("deck-web-grants-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("grants.json")
}

fn grant(id: &str, destination: &str) -> serde_json::Value {
    json!({
        "id": id, "task": "t", "gateway": "web", "operation": "connect",
        "destination": destination, "scope": "task"
    })
}

#[test]
fn a_changed_file_is_re_read_and_a_replacement_does_not_keep_an_old_permit() {
    let path = file("changed");
    std::fs::write(&path, json!([grant("a", "example.com:8443")]).to_string()).unwrap();
    let grants = Grants::new(vec![path.clone()], "t".into(), None);
    let permit = grants.web("example.com", 8443, "connect").unwrap();
    assert!(grants.web("example.com", 8444, "connect").is_none());
    assert!(grants.live(&permit));
    let original = std::fs::metadata(&path).unwrap();
    let replacement = path.with_extension("tmp");
    std::fs::write(&replacement, json!([grant("b", "example.com:8443")]).to_string()).unwrap();
    // Atomic writes can land within one filesystem clock tick. Preserve
    // that timestamp explicitly so this also reproduces on faster clocks.
    let file = std::fs::File::options().write(true).open(&replacement).unwrap();
    file.set_modified(original.modified().unwrap()).unwrap();
    assert_eq!(file.metadata().unwrap().len(), original.len());
    std::fs::rename(replacement, &path).unwrap();
    assert!(!grants.live(&permit), "a new grant does not resurrect an old permit");
    assert!(grants.web("example.com", 8443, "connect").is_some());
    std::fs::remove_file(&path).unwrap();
    assert!(grants.web("example.com", 8443, "connect").is_none());
}

#[test]
fn rules_are_scoped_to_the_task_or_project_and_operation() {
    let path = file("scope");
    let mut other_task = grant("other-task", "other.example:443");
    other_task["task"] = json!("other");
    let mut project = grant("project", "project.example:443");
    project["scope"] = json!("project");
    project["project"] = json!("/project");
    let mut other_project = project.clone();
    other_project["id"] = json!("other-project");
    other_project["project"] = json!("/other");
    other_project["destination"] = json!("other-project.example:443");
    let mut fetch = grant("fetch", "mirror.example:80");
    fetch["operation"] = json!("fetch");
    std::fs::write(&path, json!({"rules": [other_task, project, other_project, fetch]}).to_string()).unwrap();
    let grants = Grants::new(vec![path.clone()], "t".into(), Some("/project".into()));
    assert!(grants.web("other.example", 443, "connect").is_none());
    assert!(grants.web("other-project.example", 443, "connect").is_none());
    assert!(grants.web("project.example", 443, "connect").is_some());
    assert!(grants.web("project.example", 443, "fetch").is_some(), "a connection grant also covers HTTP");
    assert!(grants.web("mirror.example", 80, "fetch").is_some());
    assert!(grants.web("mirror.example", 80, "connect").is_none());
    let unconfigured = Grants::new(vec![path.clone()], "t".into(), None);
    assert!(unconfigured.web("project.example", 443, "connect").is_none());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn expired_once_and_other_gateways_grants_admit_nothing() {
    let path = file("expired");
    let mut expired = grant("expired", "expired.example:443");
    expired["expires_ms"] = json!(1);
    let mut once = grant("once", "once.example:443");
    once["scope"] = json!("once");
    let mut model = grant("model", "model.example:443");
    model["gateway"] = json!("model");
    std::fs::write(&path, json!([expired, once, model]).to_string()).unwrap();
    let grants = Grants::new(vec![path.clone()], "t".into(), None);
    for name in ["expired.example", "once.example", "model.example"] {
        assert!(grants.web(name, 443, "connect").is_none(), "{name}");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_revocation_between_admission_and_registration_refuses_the_operation() {
    let path = file("register");
    std::fs::write(&path, json!([grant("a", "example.com:443")]).to_string()).unwrap();
    let grants = Arc::new(Grants::new(vec![path.clone()], "t".into(), None));
    let permit = grants.web("example.com", 443, "connect").unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(
        matches!(grants.register(Some(permit), || {}), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn model_grants_match_exact_names_operations_and_the_decided_scope() {
    let path = file("model");
    let mut model = grant("model", "provider/model");
    model["gateway"] = json!("model");
    model["operation"] = json!("chat_completions");
    model["credential"] = json!("KEY");
    std::fs::write(&path, json!([model]).to_string()).unwrap();
    let grants = Grants::new(vec![path], "t".into(), Some("/project".into()));
    assert!(grants.model("provider/model", "KEY", None).is_some());
    assert!(grants.model("provider/model", "OTHER_KEY", None).is_none());
    assert!(grants.model("provider/model", "KEY", Some("task")).is_some());
    assert!(grants.model("provider/model", "KEY", Some("project")).is_none());
    assert!(grants.model("provider/Model", "KEY", None).is_none());
    assert!(grants.model("provider/model/other", "KEY", None).is_none());
    assert!(grants.web("provider", 443, "connect").is_none());
}

fn github_grant(id: &str, operation: &str, destination: &str, credential: &str, scope: &str) -> serde_json::Value {
    json!({
        "id": id, "task": "t", "gateway": "github", "operation": operation,
        "destination": destination, "credential": credential, "scope": scope
    })
}

#[test]
fn github_grants_match_the_admitted_operation_destination_credential_and_scope() {
    let path = file("github");
    let exact = github_grant("exact", "create_pr", "o/r#deck/t1", "KEY", "task");
    let mut project = github_grant("project", "create_pr", "o/r", "KEY", "project");
    project["task"] = json!(null);
    project["project"] = json!("/project");
    let mut expired = github_grant("expired", "create_pr", "o/r#deck/t2", "KEY", "timed");
    expired["expires_ms"] = json!(1);
    std::fs::write(&path, json!([exact, project, expired]).to_string()).unwrap();
    let grants = Grants::new(vec![path], "t".into(), Some("/project".into()));
    assert!(grants.github("create_pr", "o/r#deck/t1", Some("KEY"), "task").is_some());
    assert!(grants.github("create_pr", "o/r#deck/other", Some("KEY"), "project").is_some());
    assert!(
        grants.github("create_pr", "o/r#deck/t2", Some("KEY"), "timed").is_none(),
        "an expired rule admits nothing"
    );
    assert!(grants.github("create_pr", "o/rx#a", Some("KEY"), "project").is_none(), "the prefix ends at a name");
    assert!(grants.github("create_pr", "o/r", Some("KEY"), "project").is_none(), "a project rule covers details only");
    assert!(grants.github("create_pr", "o/r#deck/t1", Some("OTHER"), "task").is_none());
    assert!(grants.github("update_pr", "o/r#deck/t1", Some("KEY"), "task").is_none());
}

#[test]
fn legacy_model_rules_cannot_authorize_a_credential_and_web_rules_cannot_change_binding() {
    let path = file("credential-legacy");
    let mut model = grant("model", "m");
    model["gateway"] = json!("model");
    model["operation"] = json!("chat_completions");
    let mut web = grant("web", "example.com:443");
    web["credential"] = json!("KEY");
    std::fs::write(&path, json!([model, web]).to_string()).unwrap();
    let grants = Grants::new(vec![path.clone()], "t".into(), None);
    assert!(grants.model("m", "KEY", None).is_none());
    assert!(grants.web("example.com", 443, "connect").is_none());
    std::fs::remove_file(path).unwrap();
}
