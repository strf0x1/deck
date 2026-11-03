//! Guest entry point for the egress relay: bind a loopback TCP listener and
//! carry every connection to one host vsock port, optionally naming the
//! gateway kind first (the boundary status endpoint on the extension port).
//!
//! The design choice: no configuration file and no policy. The guest entry
//! starts it before the agent, and a connection it cannot carry is refused
//! rather than sent somewhere else.

use deck_egress::vsock::{self, Address, HOST_CID};
use std::io::{self, Write as _};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_PORT: u32 = 1026;
/// The extension port's key frame: a 4-byte big-endian length, then the kind.
const MAX_KIND: usize = 4 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("deck-egress: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let (listen, address, kind) = parse(std::env::args().skip(1)).map_err(io::Error::other)?;
    let listener = TcpListener::bind(&listen)?;
    let named = kind.as_deref().map(|kind| format!(" kind={kind}")).unwrap_or_default();
    println!("deck-egress: {} -> cid {} port {}{named}", listener.local_addr()?, address.cid, address.port);
    io::stdout().flush()?;
    deck_egress::serve(listener, move || {
        let mut host = vsock::connect(address)?;
        if let Some(kind) = &kind {
            write_kind(&mut host, kind)?;
        }
        Ok(host)
    })
}

/// Name the gateway kind the extension port should dispatch. The host reads
/// this frame before the stream becomes bytes.
fn write_kind(host: &mut UnixStream, kind: &str) -> io::Result<()> {
    if kind.len() > MAX_KIND {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "kind name is too long"));
    }
    host.write_all(&u32::try_from(kind.len()).unwrap_or(0).to_be_bytes())?;
    host.write_all(kind.as_bytes())?;
    host.flush()
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<(String, Address, Option<String>), String> {
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut address = Address { cid: HOST_CID, port: DEFAULT_PORT };
    let mut kind = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => listen = args.next().ok_or("--listen needs a value")?,
            "--vsock" => {
                address = Address::parse(&args.next().ok_or("--vsock needs a value")?)?;
            }
            "--kind" => kind = Some(args.next().ok_or("--kind needs a value")?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok((listen, address, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn defaults_are_the_guest_contract() {
        assert_eq!(parse(args(&[])).unwrap(), (DEFAULT_LISTEN.into(), Address { cid: 2, port: 1026 }, None));
    }

    #[test]
    fn flags_override_the_defaults() {
        let (listen, address, kind) =
            parse(args(&["--listen", "127.0.0.1:9000", "--vsock", "2:1030", "--kind", "boundary"])).unwrap();
        assert_eq!(listen, "127.0.0.1:9000");
        assert_eq!(address, Address { cid: 2, port: 1030 });
        assert_eq!(kind.as_deref(), Some("boundary"));
    }

    #[cfg(unix)]
    #[test]
    fn a_kind_is_a_length_framed_name() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            write_kind(&mut left, "boundary").unwrap();
            let mut length = [0u8; 4];
            use std::io::Read as _;
            let mut peer = right;
            peer.read_exact(&mut length).unwrap();
            let mut name = vec![0; u32::from_be_bytes(length) as usize];
            peer.read_exact(&mut name).unwrap();
            String::from_utf8(name).unwrap()
        });
        assert_eq!(writer.join().unwrap(), "boundary");
    }

    #[test]
    fn a_malformed_flag_is_refused() {
        assert!(parse(args(&["--vsock", "nope"])).is_err());
        assert!(parse(args(&["--unknown"])).is_err());
    }
}
