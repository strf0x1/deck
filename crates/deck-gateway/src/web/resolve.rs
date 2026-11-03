//! Host resolution for the web gateway (ADR 0004 §3).
//!
//! The design choice: names resolve on the host and nowhere else, and a
//! profile or a test may pin a name in a host file the gateway reads itself.
//! The guest has no resolver and no route for one, so a name that only the
//! guest's `/etc/hosts` knows resolves nowhere here. Every address a name
//! returns is classified before it is used, and a name with any refused
//! address is refused whole: dialling the remaining addresses would make the
//! decision depend on which one connected first.

use crate::web::policy::{self, Address};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A name is looked up at most this often; a burst of connections to one host
/// must not become a burst of queries, and a short cache is what keeps a
/// package install from paying for one lookup per object.
const CACHE: Duration = Duration::from_secs(30);
/// How long a resolver may take. A host that cannot answer in this window is
/// an outage for one request, not a reason to hold the connection open.
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Resolver {
    hosts: HashMap<String, Vec<IpAddr>>,
    cache: Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>,
    /// One address a case may substitute after the policy check. It exists so
    /// a test can prove the tunnel path without a route to a public host; the
    /// binary never sets it, so production resolution cannot reach it.
    redirect: Option<IpAddr>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").field("hosts", &self.hosts.keys().collect::<Vec<_>>()).finish()
    }
}

impl Resolver {
    /// A resolver with no pinned names: lookups use the host's resolver only.
    pub fn new() -> Arc<Self> {
        Self::at(None)
    }

    /// A resolver that also reads `hosts` (the shape of `/etc/hosts`: an
    /// address, whitespace, then names). The files a pinned entry names win
    /// over the host's resolver, so a profile or a test is deterministic.
    pub fn at(hosts: Option<&Path>) -> Arc<Self> {
        let mut pinned: HashMap<String, Vec<IpAddr>> = HashMap::new();
        if let Some(path) = hosts
            && let Ok(text) = std::fs::read_to_string(path)
        {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or_default();
                let mut words = line.split_whitespace();
                let Some(address) = words.next().and_then(|value| value.parse::<IpAddr>().ok()) else { continue };
                for name in words {
                    pinned.entry(name.to_ascii_lowercase()).or_default().push(address);
                }
            }
        }
        Arc::new(Self { hosts: pinned, cache: Mutex::new(HashMap::new()), redirect: None })
    }

    /// A resolver for a test that dials its own listener for a name pinned to
    /// a public address. Not reachable from the binary: no flag sets it.
    #[doc(hidden)]
    pub fn redirect_to(&self, address: IpAddr) -> Arc<Self> {
        Arc::new(Self { hosts: self.hosts.clone(), cache: Mutex::new(HashMap::new()), redirect: Some(address) })
    }

    /// What a dial actually connects to for an address the policy admitted.
    pub fn dialled(&self, address: IpAddr) -> IpAddr {
        self.redirect.unwrap_or(address)
    }

    /// The addresses to dial for `name`, or why the name has none on this
    /// host. The order is the resolver's; the policy decides which are used.
    pub fn resolve(&self, name: &str, port: u16) -> Result<Vec<IpAddr>, Refused> {
        let name = name.to_ascii_lowercase();
        if let Some(addresses) = self.hosts.get(&name) {
            return policy::is_host(&name).then(|| addresses.clone()).ok_or_else(|| Refused::Invalid(name.clone()));
        }
        if let Ok(address) = name.parse::<IpAddr>() {
            return Ok(vec![address]);
        }
        if !policy::is_host(&name) {
            return Err(Refused::Invalid(name));
        }
        if let Some(addresses) = self.cached(&name) {
            return Ok(addresses);
        }
        let addresses = query(&name, port)?;
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name, (Instant::now(), addresses.clone()));
        Ok(addresses)
    }

    /// Resolve and classify in one step. A name with no address is an outage;
    /// a name with any refused address is a policy refusal for the whole name.
    pub fn check(&self, name: &str, port: u16) -> Result<Vec<SocketAddr>, Refused> {
        let addresses = self.resolve(name, port)?;
        let mut usable = Vec::new();
        for address in addresses {
            match policy::blocked(address) {
                Some(class) => return Err(Refused::Address(name.to_owned(), address, class)),
                None => usable.push(SocketAddr::new(address, port)),
            }
        }
        if usable.is_empty() {
            return Err(Refused::Unresolved(name.to_owned()));
        }
        Ok(usable)
    }

    fn cached(&self, name: &str) -> Option<Vec<IpAddr>> {
        let cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (at, addresses) = cache.get(name)?;
        (at.elapsed() < CACHE).then(|| addresses.clone())
    }
}

/// Why a name has no address to dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The name is not a DNS name or an address, so it never reaches a lookup.
    Invalid(String),
    /// The host's resolver had no answer in time, or none at all.
    Unresolved(String),
    /// The name resolves to an address the policy never dials.
    Address(String, IpAddr, Address),
}

impl Refused {
    pub fn reason(&self) -> String {
        match self {
            Self::Invalid(name) => format!("{name:?} is not a hostname"),
            Self::Unresolved(name) => format!("{name} does not resolve on this host"),
            Self::Address(name, address, class) => {
                format!("{name} resolves to {address}, which is {}", class.reason())
            }
        }
    }
}

/// Ask the host's resolver in a bounded thread. `ToSocketAddrs` has no
/// timeout, and a hung lookup would hold the connection's thread until the
/// task ends, so the answer arrives through a channel or not at all.
fn query(name: &str, port: u16) -> Result<Vec<IpAddr>, Refused> {
    static POOL: OnceLock<()> = OnceLock::new();
    POOL.get_or_init(|| {
        // Nothing to configure: lookups use the system resolver, as every
        // other host process does.
    });
    let (done, wait) = std::sync::mpsc::channel();
    let owned = name.to_owned();
    std::thread::Builder::new()
        .name("gateway-resolve".into())
        .spawn(move || {
            let looked =
                (owned.as_str(), port).to_socket_addrs().map(|found| found.map(|addr| addr.ip()).collect::<Vec<_>>());
            let _ = done.send(looked);
        })
        .map_err(|_| Refused::Unresolved(name.to_owned()))?;
    match wait.recv_timeout(TIMEOUT) {
        Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
        Ok(_) => Err(Refused::Unresolved(name.to_owned())),
        Err(_) => Err(Refused::Unresolved(name.to_owned())),
    }
}

/// The host address this process would use for a name, for a message that
/// explains where a request went.
pub fn reachable(address: SocketAddr) -> bool {
    UdpSocket::bind(("0.0.0.0", 0)).map(|socket| socket.connect(address).is_ok()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn pinned(entries: &str) -> (Arc<Resolver>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("deck-web-resolver-{}-{}", std::process::id(), entries.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(entries.as_bytes()).unwrap();
        (Resolver::at(Some(&path)), dir)
    }

    #[test]
    fn a_pinned_name_answers_and_a_refused_address_refuses_the_name() {
        let (resolver, dir) = pinned("140.82.112.3 github.com\n127.0.0.1 local.example\n");
        let ok = resolver.check("github.com", 443).unwrap();
        assert_eq!(ok, vec![SocketAddr::new("140.82.112.3".parse().unwrap(), 443)]);
        let refused = resolver.check("local.example", 443).unwrap_err();
        assert!(matches!(refused, Refused::Address(_, _, Address::Loopback)), "{refused:?}");
        assert!(refused.reason().contains("loopback"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_name_that_resolves_to_no_address_is_unresolved() {
        let (resolver, dir) = pinned("");
        let refused = resolver.check("nowhere.invalid", 443).unwrap_err();
        assert!(matches!(refused, Refused::Unresolved(_)), "{refused:?}");
        assert!(matches!(resolver.check("*", 443), Err(Refused::Invalid(_))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_redirect_substitutes_only_the_dial() {
        let (resolver, dir) = pinned("140.82.112.3 github.com\n");
        let redirected = resolver.redirect_to("127.0.0.1".parse().unwrap());
        let checked = redirected.check("github.com", 22).unwrap();
        assert_eq!(checked[0].ip(), "140.82.112.3".parse::<IpAddr>().unwrap(), "the policy sees the resolved address");
        assert_eq!(redirected.dialled(checked[0].ip()), "127.0.0.1".parse::<IpAddr>().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_address_literal_is_its_own_answer_and_still_classified() {
        let (resolver, dir) = pinned("");
        assert_eq!(
            resolver.check("127.0.0.1", 80).unwrap_err(),
            Refused::Address("127.0.0.1".into(), "127.0.0.1".parse().unwrap(), Address::Loopback)
        );
        assert_eq!(
            resolver.check("169.254.169.254", 80).unwrap_err(),
            Refused::Address("169.254.169.254".into(), "169.254.169.254".parse().unwrap(), Address::LinkLocal)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
