//! A local coordinator's control gateway (ADR 0011 §4, amended 2026-11-17;
//! contract 1.4.0 §13.2): `deck-gateway --control-listen` on host loopback,
//! started by the task's holder before the agent, with a fresh token.
//!
//! The design choice: the token is minted here from the operating system's
//! randomness for each run, handed to the gateway in its environment (never
//! an argument, which `ps` shows) and to the agent beside the URL, and
//! written nowhere else. It keeps other host processes off the surface; it
//! does not confine the agent, which runs as the user. The gateway watches
//! the holder through its stdin pipe, so it ends with the holder whichever
//! way the holder ends.

use anyhow::{Context as _, Result, bail};
use deck_acp::names::{CONTROL_TOKEN_ENV, CONTROL_URL_ENV};
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const READY_WAIT: Duration = Duration::from_secs(10);

/// The running gateway; dropping it closes the pipe the gateway watches.
pub(crate) struct ControlGateway {
    _parent: ChildStdin,
    /// `DECK_CONTROL_URL` and `DECK_CONTROL_TOKEN` for the agent.
    pub env: Vec<(String, String)>,
}

#[derive(serde::Deserialize)]
struct Ready {
    control: std::net::SocketAddr,
}

/// The gateway binary: `DECK_GATEWAY_EXECUTABLE`, which must exist, or
/// `deck-gateway` beside this executable, as the VM backend finds it. `None`
/// when no gateway was named and none was built beside `deckd`.
fn executable() -> Result<Option<PathBuf>> {
    let named = std::env::var_os(crate::vm::GATEWAY_ENV).filter(|v| !v.is_empty()).map(PathBuf::from);
    match named {
        Some(path) if path.is_file() => Ok(Some(path)),
        Some(path) => bail!("{} names the control gateway; {} does not exist", crate::vm::GATEWAY_ENV, path.display()),
        None => Ok(Some(std::env::current_exe()?.with_file_name("deck-gateway")).filter(|path| path.is_file())),
    }
}

/// 32 bytes from the operating system's randomness, as 64 hex characters:
/// 256 bits, inside the contract's `b64token` alphabet and length.
pub(crate) fn mint() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("reading randomness for the control token")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Start task `id`'s control gateway and wait for its bound address. With no
/// gateway built, the coordinator starts without the surface: its agent gets
/// no `DECK_CONTROL_URL`, which the contract reads as "controls no task",
/// and the holder's log says why.
pub(crate) fn start(data: &Path, id: &str) -> Result<Option<ControlGateway>> {
    let Some(executable) = executable()? else {
        eprintln!("deckd: coordinator {id} has no control surface: deck-gateway is not built beside deckd");
        return Ok(None);
    };
    let token = mint()?;
    let control = crate::paths::Paths::for_data(data)?.control();
    let mut child = Command::new(executable)
        .arg("--control-listen")
        .arg("127.0.0.1:0")
        .arg("--control")
        .arg(&control)
        .arg("--task")
        .arg(id)
        .arg("--exit-on-stdin-eof")
        .env(CONTROL_TOKEN_ENV, &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("launching the control gateway")?;
    let parent = child.stdin.take().context("control gateway stdin")?;
    let stdout = child.stdout.take().context("control gateway stdout")?;
    let (ready, result) = mpsc::sync_channel(1);
    std::thread::Builder::new().name("holder-control-ready".into()).spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).take(4096).read_line(&mut line);
        let _ = ready.send(read.map(|_| line));
    })?;
    let address = match result.recv_timeout(READY_WAIT) {
        Ok(Ok(line)) => serde_json::from_str::<Ready>(&line).ok().map(|ready| ready.control),
        _ => None,
    };
    let Some(address) = address.filter(|address| address.ip().is_loopback()) else {
        let _ = child.kill();
        let _ = child.wait();
        bail!("the control gateway did not report a loopback address");
    };
    // Reap it whenever it ends; it ends with this holder.
    std::thread::Builder::new().name("holder-control-gateway".into()).spawn(move || {
        let _ = child.wait();
    })?;
    Ok(Some(ControlGateway {
        _parent: parent,
        env: vec![(CONTROL_URL_ENV.into(), format!("http://{address}")), (CONTROL_TOKEN_ENV.into(), token)],
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_token_is_256_random_bits_in_the_b64token_alphabet() {
        let (a, b) = (super::mint().unwrap(), super::mint().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
