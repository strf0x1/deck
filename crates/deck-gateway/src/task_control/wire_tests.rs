use super::*;

const TOKEN: &str = "dct_q8Zr1vKcY3nT0fWb6LmE2pXhS9aJ4uGd";

fn header(value: &str) -> Vec<(String, String)> {
    vec![("authorization".into(), value.into())]
}

#[test]
fn the_token_is_checked_by_scheme_in_any_case_and_exactly_once() {
    let token = Token::new(TOKEN).unwrap();
    assert!(authorized(&header(&format!("Bearer {TOKEN}")), Some(&token)));
    assert!(authorized(&header(&format!("bearer {TOKEN}")), Some(&token)));
    assert!(!authorized(&[], Some(&token)), "missing");
    assert!(!authorized(&header(&format!("Basic {TOKEN}")), Some(&token)), "another scheme");
    assert!(!authorized(&header("Bearer dct_not_the_task_token_000000000000"), Some(&token)), "another token");
    let twice = [header(&format!("Bearer {TOKEN}")), header(&format!("Bearer {TOKEN}"))].concat();
    assert!(!authorized(&twice, Some(&token)), "more than one header");
    assert!(authorized(&header("Bearer anything-at-all"), None), "no token: the header is ignored");
    assert!(authorized(&[], None));
}

#[test]
fn a_token_is_a_b64token_of_32_to_256_characters() {
    assert!(Token::new(TOKEN).is_ok());
    assert!(Token::new(&"a".repeat(31)).is_err());
    assert!(Token::new(&"a".repeat(257)).is_err());
    assert!(Token::new(&format!("{}==", "a".repeat(32))).is_ok());
    assert!(Token::new(&format!("{} ", "a".repeat(32))).is_err());
    assert!(!format!("{:?}", Token::new(TOKEN).unwrap()).contains("dct_"), "never printed");
}

#[test]
fn the_route_refuses_in_the_contracts_order_before_the_body() {
    assert_eq!(route("POST", "/control", Some(2)), Ok(2));
    assert_eq!(route("GET", "/nowhere", Some(2)).unwrap_err().status, 404, "the path first");
    assert_eq!(route("GET", "/control", Some(2)).unwrap_err().status, 405);
    assert_eq!(route("POST", "/control", None).unwrap_err().status, 411);
    let large = route("POST", "/control", Some(MAX_BODY + 1)).unwrap_err();
    assert_eq!((large.status, large.code), (413, Code::BadRequest));
    assert_eq!(route("POST", "/control", Some(MAX_BODY)), Ok(MAX_BODY));
}

#[test]
fn every_operation_parses_with_its_fields() {
    let ok = |body: Value| parse(body.to_string().as_bytes()).unwrap();
    assert_eq!(ok(json!({ "operation": "tasks" })), Operation::Tasks { project: None });
    assert_eq!(ok(json!({ "operation": "task", "id": "t1" })), Operation::Task { id: "t1".into() });
    assert_eq!(
        ok(json!({ "operation": "create", "project": "git:p", "brief": "Do it", "budget": { "tokens": 5 } })),
        Operation::Create {
            project: Some("git:p".into()),
            brief: "Do it".into(),
            title: None,
            budget: Some(Budget { requests: None, tokens: Some(5) })
        }
    );
    assert_eq!(
        ok(json!({ "operation": "prompt", "id": "t1", "text": "go" })),
        Operation::Prompt { id: "t1".into(), text: "go".into() }
    );
    assert_eq!(
        ok(json!({ "operation": "wait", "id": "t1", "wait_ms": 300000 })),
        Operation::Wait { id: "t1".into(), after_turn: None, wait_ms: 300_000 }
    );
    assert_eq!(ok(json!({ "operation": "stop", "id": "t1" })), Operation::Stop { id: "t1".into() });
    assert_eq!(
        ok(json!({ "operation": "create", "brief": "Do it" })),
        Operation::Create { project: None, brief: "Do it".into(), title: None, budget: None },
        "project is optional since contract 1.5.0"
    );
}

#[test]
fn malformed_requests_are_bad_requests_and_other_operations_are_refused() {
    let refused = |body: &str| parse(body.as_bytes()).unwrap_err();
    let unknown = refused(r#"{"operation":"stop","id":"t","force":true}"#);
    assert_eq!((unknown.code, unknown.message.as_str()), (Code::BadRequest, "unknown field `force`"));
    let long = refused(r#"{"operation":"wait","id":"t","wait_ms":300001}"#);
    assert_eq!((long.code, long.message.as_str()), (Code::BadRequest, "wait_ms is at most 300000"));
    assert_eq!(refused(r#"{"id":"t"}"#).message, "missing `operation`");
    assert_eq!(refused(r#"{"operation":7}"#).code, Code::BadRequest);
    let outside = refused(r#"{"operation":"destroy","id":"t"}"#);
    assert_eq!((outside.code, outside.status), (Code::Refused, 403));
    assert!(outside.message.contains("tasks, task, create, prompt, wait, stop"), "{}", outside.message);
    for body in [
        "not json",
        "[]",
        r#"{"operation":"task"}"#,
        r#"{"operation":"task","id":""}"#,
        r#"{"operation":"wait","id":"t"}"#,
        r#"{"operation":"wait","id":"t","wait_ms":-1}"#,
        r#"{"operation":"create","project":"p","brief":"b","budget":{}}"#,
        r#"{"operation":"create","project":"p","brief":"b","budget":{"tokens":0}}"#,
        r#"{"operation":"create","project":"p","brief":"b","budget":{"cost":1}}"#,
    ] {
        assert_eq!(refused(body).code, Code::BadRequest, "{body}");
    }
    let title = format!(r#"{{"operation":"create","project":"p","brief":"b","title":"{}"}}"#, "é".repeat(81));
    assert_eq!(refused(&title).code, Code::BadRequest, "80 characters, not bytes");
    let title = format!(r#"{{"operation":"create","project":"p","brief":"b","title":"{}"}}"#, "é".repeat(80));
    assert!(parse(title.as_bytes()).is_ok());
}

#[test]
fn refusal_bodies_carry_the_code_twice_and_honest_retryability() {
    for code in Code::ALL {
        let body = Refusal::new(code, "why").body();
        assert_eq!(body["error"]["type"], code.code());
        assert_eq!(body["error"]["code"], code.code());
        assert_eq!(body["retryable"], json!(code.retryable()));
        assert_eq!(Code::named(code.code()), Some(code));
    }
    assert_eq!(Refusal::new(Code::Busy, "x".repeat(5000)).message.chars().count(), MESSAGE_CHARS);
}

#[test]
fn the_daemon_hears_exactly_the_typed_operation() {
    let parsed = parse(br#"{"operation":"create","project":"p","brief":"b"}"#).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), json!({ "operation": "create", "project": "p", "brief": "b" }));
    assert_eq!(
        parse(br#"{"operation":"wait","id":"t","wait_ms":1000}"#).unwrap().timeout(),
        Duration::from_millis(31_000)
    );
}
