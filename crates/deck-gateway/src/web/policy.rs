//! Which destination the web and package gateway may dial (ADR 0004 §3).
//!
//! The design choice: every check works on the address the host will dial,
//! never only on the name the guest wrote. A name the guest controls must not
//! turn into a route to the host or its network, so a name that resolves to
//! loopback, private, link-local, shared, documentation, multicast or
//! unspecified space is refused on every port and without asking anyone. A
//! profile entry is exactly one name (`host`) or one port on one name
//! (`host:port`); it is never a wildcard and never a port range.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// A CONNECT to this port needs no approval when the task holds a profile
/// entry for the name. Another port is a policy decision: the profile hint
/// does not cover it and the grant names it (§3).
pub const DEFAULT_PORT: u16 = 443;
/// The port a plain `http://` target means when it names none. Plain HTTP is
/// only for declared mirrors, and a mirror's listener is on 80.
pub const PLAIN_PORT: u16 = 80;
/// The most tunnels and plain-HTTP streams one task keeps open at once.
pub const TUNNELS: usize = 64;

/// One destination a profile may declare, parsed where the host wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowed {
    /// Every port on one hostname.
    Host(String),
    /// One port on one hostname.
    Port(String, u16),
}

impl Allowed {
    /// Parse `host`, `host:port` or `[v6]:port`. A malformed entry is not a
    /// destination, so it is dropped rather than widened.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim().to_ascii_lowercase();
        if let Some(rest) = entry.strip_prefix('[') {
            let (address, port) = rest.split_once("]:")?;
            let address: IpAddr = address.parse().ok()?;
            let port: u16 = port.parse().ok()?;
            return address.is_ipv6().then(|| Self::Port(address.to_string(), port));
        }
        if entry.matches(':').count() > 1 {
            // A bare IPv6 literal has no port to split off.
            let address: IpAddr = entry.parse().ok()?;
            return Some(Self::Host(address.to_string()));
        }
        match entry.split_once(':') {
            Some((host, port)) => {
                let port: u16 = port.parse().ok()?;
                is_host(host).then(|| Self::Port(host.to_string(), port))
            }
            None => is_host(&entry).then_some(Self::Host(entry)),
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Self::Host(host) | Self::Port(host, _) => host,
        }
    }

    /// Whether this entry admits a request to `host:port`.
    pub fn admits(&self, host: &str, port: u16) -> bool {
        match self {
            Self::Host(name) => name == host,
            Self::Port(name, allowed) => name == host && *allowed == port,
        }
    }

    /// Whether this profile entry lets a CONNECT to `host:port` through
    /// without asking: a name entry is the default port on that host, and a
    /// port entry is exactly its own port.
    pub fn covers(&self, host: &str, port: u16) -> bool {
        match self {
            Self::Host(name) => name == host && port == DEFAULT_PORT,
            Self::Port(..) => self.admits(host, port),
        }
    }

    /// The destination an approval or grant names. The port is part of it, so
    /// `example.com:22` allowed does not allow `example.com:2222`.
    pub fn destination(&self) -> String {
        match self {
            Self::Host(host) => format!("{host}:{DEFAULT_PORT}"),
            Self::Port(host, port) => format!("{host}:{port}"),
        }
    }
}

/// A name the guest may write: lowercase ascii labels joined by dots, or an
/// IP literal for a profile or a host-file entry. Anything else is refused
/// before it reaches a resolver.
pub fn is_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.ends_with('.') {
        return false;
    }
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    })
}

/// Why a destination was refused without asking a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Address {
    Loopback,
    Private,
    LinkLocal,
    Shared,
    Multicast,
    Unspecified,
    Broadcast,
    Documentation,
}

impl Address {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Private => "a private network",
            Self::LinkLocal => "a link-local address",
            Self::Shared => "shared address space",
            Self::Multicast => "multicast",
            Self::Unspecified => "an unspecified address",
            Self::Broadcast => "broadcast",
            Self::Documentation => "documentation address space",
        }
    }
}

/// Classify one host address. Anything that is not globally routable unicast
/// is refused, so a new special-purpose range fails closed instead of being
/// reachable by omission.
///
/// IPv4-mapped and NAT64 forms are unwrapped first: those are how an IPv4
/// address travels in an IPv6 one, and classifying the wrapper would let
/// `::ffff:127.0.0.1` reach loopback.
pub fn blocked(address: IpAddr) -> Option<Address> {
    inner(address)
}

fn inner(address: IpAddr) -> Option<Address> {
    match address {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                Some(Address::Loopback)
            } else if v4.is_private() {
                Some(Address::Private)
            } else if v4.is_link_local() {
                Some(Address::LinkLocal)
            } else if v4.is_unspecified() {
                Some(Address::Unspecified)
            } else if v4.is_multicast() {
                Some(Address::Multicast)
            } else if v4.is_broadcast() {
                Some(Address::Broadcast)
            } else if v4.is_documentation() {
                Some(Address::Documentation)
            } else if octets[0] == 100 && (octets[1] & 0xc0) == 64 {
                Some(Address::Shared)
            } else if octets[0] == 0 {
                Some(Address::Unspecified)
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some(Address::Loopback);
            }
            if v6.is_unspecified() {
                return Some(Address::Unspecified);
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return inner(IpAddr::V4(v4));
            }
            let segments = v6.segments();
            // NAT64 `64:ff9b::/96` carries an IPv4 address in the last two
            // segments; classify what would actually be dialled.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let embedded = std::net::Ipv4Addr::new(
                    (segments[6] >> 8) as u8,
                    segments[6] as u8,
                    (segments[7] >> 8) as u8,
                    segments[7] as u8,
                );
                return inner(IpAddr::V4(embedded));
            }
            if v6.is_multicast() {
                Some(Address::Multicast)
            } else if segments[0] & 0xfe00 == 0xfc00 {
                Some(Address::Private)
            } else if segments[0] & 0xffc0 == 0xfe80 {
                Some(Address::LinkLocal)
            } else if segments[0] == 0x2001 && segments[1] == 0x0db8 {
                Some(Address::Documentation)
            } else {
                None
            }
        }
    }
}

/// The destinations a task may reach without an approval. A package
/// manager's mirror list is host policy: the guest may name a profile, not
/// define one, and naming an undeclared profile grants nothing.
#[derive(Debug, Clone, Default)]
pub struct Profiles {
    entries: Vec<(String, Allowed)>,
}

impl Profiles {
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, S)>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        for (profile, list) in entries {
            let profile = profile.trim().to_ascii_lowercase();
            for entry in list.as_ref().lines() {
                if let Some(allowed) = Allowed::parse(entry) {
                    parsed.push((profile.clone(), allowed));
                }
            }
        }
        Self { entries: parsed }
    }

    /// A task's `--web-profile NAME` declarations.
    pub fn declared(profiles: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut entries = Vec::new();
        for name in profiles {
            let name = name.trim().to_ascii_lowercase();
            match crate::web::profiles::entries(&name) {
                Some(list) => entries.extend(list.iter().map(|entry| (name.clone(), (*entry).to_string()))),
                None => return Err(format!("{name:?} is not a profile this gateway defines")),
            }
        }
        Ok(Self::new(entries))
    }

    pub fn entries(&self) -> impl Iterator<Item = &(String, Allowed)> {
        self.entries.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The profile that pre-approves a CONNECT to this name and port, if any.
    pub fn covering(&self, host: &str, port: u16) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, allowed)| allowed.host() == host && allowed.covers(host, port))
            .map(|(name, _)| name.as_str())
    }

    /// Every destination of one profile, for a project rule.
    pub fn of(&self, profile: &str) -> Vec<String> {
        self.entries.iter().filter(|(name, _)| name == profile).map(|(_, allowed)| allowed.destination()).collect()
    }
}

/// How many tunnels this process has open. A task that held every tunnel open
/// could exhaust the gateway's and the host's sockets, so the slot is taken
/// before a connection is dialled and released when the tunnel ends.
pub struct Tunnels {
    active: Mutex<usize>,
}

impl Tunnels {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { active: Mutex::new(0) })
    }

    /// Take a slot, or `None` when the task already holds [`TUNNELS`].
    pub fn open(self: &Arc<Self>) -> Option<Tunnel> {
        let mut active = self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *active >= TUNNELS {
            return None;
        }
        *active += 1;
        Some(Tunnel { tunnels: self.clone() })
    }
}

pub struct Tunnel {
    tunnels: Arc<Tunnels>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let mut active = self.tunnels.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn host_and_private_space_is_refused_and_public_space_is_not() {
        for (value, class) in [
            ("127.0.0.1", Address::Loopback),
            ("127.9.9.9", Address::Loopback),
            ("10.0.0.1", Address::Private),
            ("172.16.5.4", Address::Private),
            ("192.168.1.1", Address::Private),
            ("169.254.169.254", Address::LinkLocal),
            ("0.0.0.0", Address::Unspecified),
            ("224.0.0.1", Address::Multicast),
            ("255.255.255.255", Address::Broadcast),
            ("100.64.0.1", Address::Shared),
            ("192.0.2.1", Address::Documentation),
            ("::1", Address::Loopback),
            ("fe80::1", Address::LinkLocal),
            ("fc00::1", Address::Private),
            ("fd12::1", Address::Private),
            ("::", Address::Unspecified),
            ("ff02::1", Address::Multicast),
            ("2001:db8::1", Address::Documentation),
            ("::ffff:127.0.0.1", Address::Loopback),
            ("::ffff:10.0.0.1", Address::Private),
            ("64:ff9b::a00:1", Address::Private),
        ] {
            assert_eq!(blocked(ip(value)), Some(class), "{value} must be refused");
        }
        for value in ["140.82.112.3", "1.1.1.1", "2606:4700::1111", "8.8.8.8"] {
            assert_eq!(blocked(ip(value)), None, "{value} is a public destination");
        }
    }

    #[test]
    fn a_profile_entry_is_a_name_or_one_port_never_a_wildcard() {
        assert_eq!(Allowed::parse("github.com"), Some(Allowed::Host("github.com".into())));
        assert_eq!(Allowed::parse(" GitHub.COM "), Some(Allowed::Host("github.com".into())));
        assert_eq!(Allowed::parse("example.com:8443"), Some(Allowed::Port("example.com".into(), 8443)));
        assert_eq!(Allowed::parse("[2606:4700::1111]:443"), Some(Allowed::Port("2606:4700::1111".into(), 443)));
        assert_eq!(Allowed::parse("2606:4700::1111"), Some(Allowed::Host("2606:4700::1111".into())));
        for bad in ["", "*", "*.example.com", "example.com:https", "example.com/", ".com", "a..b", "-a.com"] {
            assert_eq!(Allowed::parse(bad), None, "{bad} must not be a destination");
        }
    }

    #[test]
    fn a_profile_covers_443_and_another_port_is_a_new_decision() {
        let profiles = Profiles::new([("repo".to_string(), "example.com")]);
        assert_eq!(profiles.covering("example.com", 443), Some("repo"));
        assert_eq!(profiles.covering("example.com", 8443), None);
        assert_eq!(profiles.covering("sub.example.com", 443), None);
        let pinned = Profiles::new([("ssh".to_string(), "example.com:22")]);
        assert_eq!(pinned.covering("example.com", 22), Some("ssh"));
        assert_eq!(pinned.covering("example.com", 80), None, "a port entry is exactly its port");
        assert_eq!(pinned.of("ssh"), vec!["example.com:22".to_string()]);
    }

    #[test]
    fn a_declared_profile_is_a_built_in_name_and_nothing_else() {
        let repo = Profiles::declared(["repo".to_string()]).unwrap();
        assert_eq!(repo.covering("raw.githubusercontent.com", 443), Some("repo"));
        assert_eq!(repo.covering("example.com", 443), None, "a profile lists names, never a pattern");
        assert!(Profiles::declared(["invented".to_string()]).is_err());
        assert!(Profiles::declared(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn tunnels_are_bounded() {
        let tunnels = Tunnels::new();
        let held: Vec<Tunnel> = (0..TUNNELS).filter_map(|_| tunnels.open()).collect();
        assert_eq!(held.len(), TUNNELS);
        assert!(tunnels.open().is_none(), "a task cannot hold more than its limit");
        drop(held);
        assert!(tunnels.open().is_some(), "a finished tunnel releases its slot");
    }
}
