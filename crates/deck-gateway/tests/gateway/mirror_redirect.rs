//! Redirect refusals through the real gateway. An unusable Location must
//! produce the boundary's bounded denial without reaching a second upstream.

use super::*;

#[test]
fn unusable_mirror_redirects_are_refused_instead_of_relayed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let locations = [
        Some(format!("https://mirror.example:{port}/target")),
        Some("ftp://mirror.example/file".into()),
        Some("//elsewhere.example/file".into()),
        Some("http:///missing-host".into()),
        Some("http://mirror.example:bad/file".into()),
        Some("relative-file".into()),
        Some(String::new()),
        None,
    ];
    let count = locations.len();
    let fixture = std::thread::spawn(move || {
        for (index, location) in locations.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let request = http::read(&mut BufReader::new(stream.try_clone().unwrap())).unwrap().unwrap();
            assert_eq!(request.target, format!("/case-{index}"), "a redirect target must never be dialled");
            let location = location.map(|value| format!("Location: {value}\r\n")).unwrap_or_default();
            write!(stream, "HTTP/1.1 302 Found\r\n{location}Content-Length: 17\r\n\r\nredirect-sentinel").unwrap();
        }
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no second destination should be dialled");
    });
    let hosts = hosts_file("mirror-redirect-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    pin(&hosts, "140.82.112.4", "elsewhere.example");
    let grants = grant_file("mirror-redirect-grants", &format!("mirror.example:{port}"), "fetch");
    let gateway = with_web("mirror-redirect-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.as_ref().unwrap();
    for index in 0..count {
        let response = send(
            web,
            &format!("GET http://mirror.example:{port}/case-{index} HTTP/1.1\r\nhost: mirror.example:{port}\r\n\r\n"),
        );
        assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"), "case {index}: {response}");
        assert!(response.contains("deck_boundary_denied"), "{response}");
        assert!(!response.contains("redirect-sentinel"), "an upstream redirect must not escape: {response}");
        assert!(!response.contains("approval_id"), "an unusable target cannot be approved: {response}");
        assert!(response.len() < 2048, "the denial is bounded");
    }
    fixture.join().unwrap();
}
