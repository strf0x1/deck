//! Exact fixed-length mirror bodies through the real gateway, directly and
//! after an approved redirect. The fixture sends headers and binary payload
//! together so the client can check framing as well as every payload byte.

use super::*;

#[test]
fn mirror_bodies_are_exact_directly_and_after_a_relative_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = b"mirror-ok\0\xff\r\n";
    let fixture = std::thread::spawn(move || {
        for path in ["/direct", "/moved", "/final", "/empty"] {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let request = http::read(&mut BufReader::new(stream.try_clone().unwrap())).unwrap().unwrap();
            assert_eq!(request.target, path);
            let response = if path == "/moved" {
                b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 5\r\n\r\nmoved".to_vec()
            } else {
                let payload: &[u8] = if path == "/empty" { b"" } else { body };
                let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", payload.len()).into_bytes();
                response.extend_from_slice(payload);
                response
            };
            stream.write_all(&response).unwrap();
        }
    });
    let hosts = hosts_file("mirror-body-hosts");
    pin(&hosts, "140.82.112.3", "mirror.example");
    let grants = grant_file("mirror-body-grants", &format!("mirror.example:{port}"), "fetch");
    let gateway = with_web("mirror-body-gw", &[&grants], &[], Some(&hosts), None);
    let web = gateway.ready.web.as_ref().unwrap();
    for path in ["/direct", "/moved", "/empty"] {
        let mut stream = UnixStream::connect(web).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(stream, "GET http://mirror.example:{port}{path} HTTP/1.1\r\nhost: mirror.example:{port}\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let end = response.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&response[..end]);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        let expected: &[u8] = if path == "/empty" { b"" } else { body };
        assert!(head.contains(&format!("content-length: {}\r\n", expected.len())), "{head}");
        assert_eq!(&response[end..], expected, "{path}: the body must match exactly through EOF");
    }
    fixture.join().unwrap();
}
