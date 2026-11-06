//! Project revocation must finish narrowing access before auditing it, so an
//! audit failure cannot preserve the project rule or another task's grant.

use super::*;

fn request(task: &str, project: &str, destination: &str) -> Incoming {
    Incoming {
        task: task.into(),
        project: project.into(),
        session: None,
        gateway: "web".into(),
        operation: "connect".into(),
        destination: destination.into(),
        credential: None,
        details: Details::default(),
        reason: None,
        coverage: None,
        approvals: None,
    }
}

#[test]
fn a_project_revocation_finishes_before_reporting_an_audit_failure() {
    let dir = std::env::temp_dir().join(format!("deck-project-revoke-audit-{}-{}", std::process::id(), now_ms()));
    fs::create_dir_all(&dir).unwrap();
    let boundary = Boundary::open(&dir).unwrap();
    for task in ["first", "peer"] {
        for destination in ["revoked.example:443", "kept.example:443"] {
            let item = boundary.submit(request(task, "/project", destination)).unwrap();
            boundary.decide(&item.id, Decision::Task).unwrap();
        }
    }
    let other = boundary.submit(request("other", "/other", "revoked.example:443")).unwrap();
    boundary.decide(&other.id, Decision::Task).unwrap();
    boundary.profile(Some("/project"), &["revoked.example:443".into()], false).unwrap();
    let rule = boundary
        .access_grants("first", "/project")
        .unwrap()
        .into_iter()
        .find(|grant| grant.scope == Scope::Project)
        .unwrap();
    fs::create_dir_all(dir.join("approvals/audit.jsonl")).unwrap();

    assert!(boundary.revoke_access_grant("first", "/project", &["first".into(), "peer".into()], &rule.id).is_err());
    let check_access = |boundary: &Boundary| {
        for task in ["first", "peer"] {
            let grants = boundary.access_grants(task, "/project").unwrap();
            assert_eq!(grants.len(), 1, "only the unrelated grant remains for {task}");
            assert_eq!(grants[0].destination, "kept.example:443");
            assert_eq!(grants[0].scope, Scope::Task);
            assert_eq!(
                boundary.submit(request(task, "/project", "revoked.example:443")).unwrap().state,
                State::Pending
            );
            assert_eq!(
                boundary.submit(request(task, "/project", "kept.example:443")).unwrap().state,
                State::Allowed(Scope::Task)
            );
        }
        assert_eq!(
            boundary.submit(request("other", "/other", "revoked.example:443")).unwrap().state,
            State::Allowed(Scope::Task)
        );
    };
    check_access(&boundary);
    drop(boundary);
    check_access(&Boundary::open(&dir).unwrap());
    fs::remove_dir_all(dir).unwrap();
}
