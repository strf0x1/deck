//! The real GitHub gateway sends host-generated content identities and the
//! credential name to the queue, without sending pull request bodies.

use super::*;

#[test]
fn github_approval_identity_tracks_publish_and_pull_request_edits() {
    let (dir, remote) = bare("gh-identity");
    let (api, upstream, _) = api(|_| r#"{"head":{"ref":"deck/t-1","repo":{"full_name":"owner/repo"}}}"#.into());
    let (queue, seen) =
        interactive_queue("gh-identity-queue", |_| serde_json::json!({ "id": "review", "state": "pending" }));
    let gateway = with_github("gh-identity-gw", &api, &url(&remote), Some((&queue, "t-1")), Some("host-token"));
    let github = gateway.ready.github.as_ref().unwrap();
    let first = bundle(&dir, "deck/t-1", "first", &dir.join("none"));
    let second = bundle(&dir, "deck/t-1", "other", &dir.join("none"));
    for bytes in [&first, &first, &second] {
        assert!(publish(github, "deck/t-1", bytes).starts_with("HTTP/1.1 403 Forbidden"));
    }
    for operation in ["create_pr", "update_pr"] {
        for body in ["draft-one", "draft-two"] {
            let ask = match operation {
                "create_pr" => serde_json::json!({
                    "operation": operation, "head": "deck/t-1", "base": "main", "title": "Reviewed title", "body": body,
                }),
                _ => {
                    serde_json::json!({ "operation": operation, "number": 7, "title": "Reviewed title", "body": body })
                }
            };
            let path =
                if operation == "create_pr" { "/operations/pull_request" } else { "/operations/pull_request/update" };
            let response = send(
                github,
                &format!("POST {path} HTTP/1.1\r\ncontent-length: {}\r\n\r\n{ask}", ask.to_string().len()),
            );
            assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
        }
    }
    // The identity is the host's: a guest that names one is refused before
    // anyone is asked (requests are closed, contract 1.2.0 §11.2).
    let forged = r#"{"operation":"create_pr","head":"deck/t-1","base":"main","title":"t","identity":"guest"}"#;
    let response = send(
        github,
        &format!("POST /operations/pull_request HTTP/1.1\r\ncontent-length: {}\r\n\r\n{forged}", forged.len()),
    );
    assert!(response.starts_with("HTTP/1.1 400") && response.contains("deck_bad_request"), "{response}");
    let seen = seen.lock().unwrap();
    let approvals: Vec<_> = seen.iter().filter(|request| request["op"] == "approval").collect();
    assert_eq!(approvals.len(), 7);
    let identities: Vec<_> = approvals.iter().map(|request| request["details"]["identity"].as_str().unwrap()).collect();
    assert_eq!(identities[0], identities[1], "an identical publish can retry");
    assert_ne!(identities[0], identities[2], "another commit needs another Once approval");
    assert_ne!(identities[3], identities[4], "a changed PR body needs another Once approval");
    assert_ne!(identities[5], identities[6], "a changed PR edit needs another Once approval");
    for (approval, identity) in approvals.iter().zip(identities) {
        assert_eq!(approval["credential"], "DECK_GATEWAY_TEST_GITHUB");
        assert!(identity.starts_with("sha256:") && identity.len() == 71, "{identity}");
        let record = approval.to_string();
        assert!(
            !record.contains("draft-one") && !record.contains("draft-two") && !record.contains("host-token"),
            "{record}"
        );
    }
    let upstream = upstream.lock().unwrap();
    assert_eq!(upstream.len(), 9, "each covered write reads visibility, and each update reads its PR");
    assert!(upstream.iter().all(|request| request.method == "GET"));
}
