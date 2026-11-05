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
    let permit = grants.authorize("example.com", 8443, "connect").unwrap();
    assert!(grants.authorize("example.com", 8444, "connect").is_none());
    assert!(grants.live(&permit));
    let replacement = path.with_extension("tmp");
    std::fs::write(&replacement, json!([grant("b", "example.com:8443")]).to_string()).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    assert!(!grants.live(&permit), "a new grant does not resurrect an old permit");
    assert!(grants.authorize("example.com", 8443, "connect").is_some());
    std::fs::remove_file(&path).unwrap();
    assert!(grants.authorize("example.com", 8443, "connect").is_none());
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
    assert!(grants.authorize("other.example", 443, "connect").is_none());
    assert!(grants.authorize("other-project.example", 443, "connect").is_none());
    assert!(grants.authorize("project.example", 443, "connect").is_some());
    assert!(grants.authorize("project.example", 443, "fetch").is_some(), "a connection grant also covers HTTP");
    assert!(grants.authorize("mirror.example", 80, "fetch").is_some());
    assert!(grants.authorize("mirror.example", 80, "connect").is_none());
    let unconfigured = Grants::new(vec![path.clone()], "t".into(), None);
    assert!(unconfigured.authorize("project.example", 443, "connect").is_none());
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
        assert!(grants.authorize(name, 443, "connect").is_none(), "{name}");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_revocation_between_admission_and_registration_refuses_the_operation() {
    let path = file("register");
    std::fs::write(&path, json!([grant("a", "example.com:443")]).to_string()).unwrap();
    let grants = Arc::new(Grants::new(vec![path.clone()], "t".into(), None));
    let permit = grants.authorize("example.com", 443, "connect").unwrap();
    std::fs::remove_file(path).unwrap();
    let (guest, _peer) = UnixStream::pair().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_accepted, _) = listener.accept().unwrap();
    assert!(
        matches!(grants.register(Some(permit), &guest, &upstream), Err(error) if error.kind() == io::ErrorKind::PermissionDenied)
    );
}
