//! The guest-side relay for deck's egress gateways (ADR 0004).
//!
//! The design choice: this is a byte carrier, not a policy point. It listens
//! on guest loopback for clients that cannot speak AF_VSOCK and opens one
//! host vsock connection per TCP connection. An agent can replace this
//! process or open the vsock port itself; the host gateway applies the same
//! policy either way, so bypassing the relay widens nothing.

pub mod vsock;

use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;

/// Accept forever, bridging each connection to `connect()`. The connector is
/// a parameter so the relay can be tested against a Unix socket on a host
/// without AF_VSOCK; in the guest it is [`vsock::connect`].
pub fn serve<F>(listener: TcpListener, connect: F) -> io::Result<()>
where
    F: Fn() -> io::Result<UnixStream> + Send + Sync + 'static,
{
    let connect = std::sync::Arc::new(connect);
    for incoming in listener.incoming() {
        let tcp = incoming?;
        let connect = connect.clone();
        std::thread::Builder::new().name("deck-egress".into()).spawn(move || {
            match connect() {
                Ok(host) => {
                    if let Err(error) = bridge(tcp, host) {
                        eprintln!("deck-egress: connection ended: {error}");
                    }
                }
                Err(error) => {
                    // No host route or the gateway is gone: fail this
                    // connection, never fake one.
                    eprintln!("deck-egress: host connection failed: {error}");
                    let _ = tcp.shutdown(Shutdown::Both);
                }
            }
        })?;
    }
    Ok(())
}

/// Copy both directions until both are finished. A shutdown from either side
/// wakes the other direction, so a half-closed client cannot pin a thread.
fn bridge(tcp: TcpStream, host: UnixStream) -> io::Result<()> {
    let tcp_reader = tcp.try_clone()?;
    let host_reader = host.try_clone()?;
    let wake_tcp = tcp.try_clone()?;
    let wake_host = host.try_clone()?;
    std::thread::scope(|scope| {
        let (finished, first) = std::sync::mpsc::channel();
        let other = finished.clone();
        let up = scope.spawn(move || {
            let mut reader = &tcp_reader;
            let mut writer = &host;
            let outcome = io::copy(&mut reader, &mut writer);
            let _ = finished.send(());
            outcome
        });
        let down = scope.spawn(move || {
            let mut reader = &host_reader;
            let mut writer = &tcp;
            let outcome = io::copy(&mut reader, &mut writer);
            let _ = other.send(());
            outcome
        });
        // Whichever direction ends first closes both, so a client that keeps
        // its write side open cannot pin the other thread.
        let _ = first.recv();
        let _ = wake_tcp.shutdown(Shutdown::Both);
        let _ = wake_host.shutdown(Shutdown::Both);
        let _ = up.join();
        let _ = down.join();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn relays_bytes_in_both_directions() {
        let dir = std::env::temp_dir().join(format!("deck-egress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host_path = dir.join("host.sock");
        let host = std::os::unix::net::UnixListener::bind(&host_path).unwrap();
        let host = std::thread::spawn(move || {
            let (mut stream, _) = host.accept().unwrap();
            let mut request = [0u8; 5];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"guest");
            stream.write_all(b"host!").unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let relay = std::thread::spawn({
            let host_path = host_path.clone();
            move || serve(listener, move || UnixStream::connect(&host_path))
        });

        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"guest").unwrap();
        let mut reply = [0u8; 5];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"host!");
        drop(client);
        host.join().unwrap();
        drop(relay);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
