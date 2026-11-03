//! Guest entry point for the egress relay: bind a loopback TCP listener and
//! carry every connection to one host vsock port.
//!
//! The design choice: no configuration file and no policy. The guest entry
//! starts it before the agent, and a connection it cannot carry is refused
//! rather than sent somewhere else.

use deck_egress::vsock::{self, Address, HOST_CID};
use std::io::{self, Write as _};
use std::net::TcpListener;
use std::process::ExitCode;

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_PORT: u32 = 1026;

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
    let (listen, address) = parse(std::env::args().skip(1)).map_err(io::Error::other)?;
    let listener = TcpListener::bind(&listen)?;
    println!("deck-egress: {} -> cid {} port {}", listener.local_addr()?, address.cid, address.port);
    io::stdout().flush()?;
    deck_egress::serve(listener, move || vsock::connect(address))
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<(String, Address), String> {
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut address = Address { cid: HOST_CID, port: DEFAULT_PORT };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => listen = args.next().ok_or("--listen needs a value")?,
            "--vsock" => {
                address = Address::parse(&args.next().ok_or("--vsock needs a value")?)?;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok((listen, address))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn defaults_are_the_guest_contract() {
        assert_eq!(parse(args(&[])).unwrap(), (DEFAULT_LISTEN.into(), Address { cid: 2, port: 1026 }));
    }

    #[test]
    fn flags_override_the_defaults() {
        let (listen, address) = parse(args(&["--listen", "127.0.0.1:9000", "--vsock", "2:1030"])).unwrap();
        assert_eq!(listen, "127.0.0.1:9000");
        assert_eq!(address, Address { cid: 2, port: 1030 });
    }

    #[test]
    fn a_malformed_flag_is_refused() {
        assert!(parse(args(&["--vsock", "nope"])).is_err());
        assert!(parse(args(&["--unknown"])).is_err());
    }
}
