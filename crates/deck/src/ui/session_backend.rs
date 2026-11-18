//! Where New session's task runs (ADR 0016): `deckd` offers its backends in
//! the settings snapshot, the default first, and the header's menu picks one.
//!
//! The design choice: only a pick other than the default is sent with a
//! create, as with the model, so a task without one gets whatever `deckd`
//! defaults to when it is created; a pick `deckd` stops offering is dropped.

/// New session's backend pick. No state beyond the pick itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionBackend {
    pick: Option<String>,
}

impl SessionBackend {
    /// What New session runs on: the pick while offered, else the default.
    pub fn current(&self, offered: &[String]) -> Option<String> {
        self.request(offered).or_else(|| offered.first().cloned())
    }

    /// The backend to name in a create: only a pick `deckd` still offers.
    pub fn request(&self, offered: &[String]) -> Option<String> {
        self.pick.clone().filter(|pick| offered.contains(pick))
    }

    /// Pick `name`; picking the default clears the pick.
    pub fn pick(&mut self, name: &str, offered: &[String]) {
        self.pick = (offered.first().map(String::as_str) != Some(name)).then(|| name.to_owned());
    }
}

/// What a backend's row says: its name and what it promises.
pub fn describe(name: &str) -> &'static str {
    match name {
        "vm" => "sealed microVM, host gateways",
        "local" => "on this machine, construct's sandbox",
        _ => "",
    }
}

/// The header chip's words: where New session runs, while there is a
/// choice, then the model it starts on; nothing when there is neither.
pub fn chip_label(offered: &[String], backend: Option<String>, model: Option<String>) -> Option<String> {
    let place = backend.filter(|_| offered.len() > 1);
    let parts: Vec<String> = place.into_iter().chain(model).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_default_is_never_sent_and_a_pick_is_until_deckd_stops_offering_it() {
        let both = offered(&["vm", "local"]);
        let mut backend = SessionBackend::default();
        assert_eq!(backend.current(&both).as_deref(), Some("vm"), "deckd's default first");
        assert_eq!(backend.request(&both), None);

        backend.pick("local", &both);
        assert_eq!(backend.current(&both).as_deref(), Some("local"));
        assert_eq!(backend.request(&both).as_deref(), Some("local"));

        let local_only = offered(&["local"]);
        backend.pick("local", &both);
        assert_eq!(backend.request(&offered(&["vm"])), None, "a pick deckd no longer offers is not sent");
        assert_eq!(backend.current(&local_only).as_deref(), Some("local"));

        backend.pick("vm", &both);
        assert_eq!(backend.request(&both), None, "picking the default clears the pick");
        assert_eq!(backend.current(&[]), None, "nothing before the snapshot");
    }

    #[test]
    fn the_chip_names_the_backend_only_while_there_is_a_choice() {
        let both = offered(&["vm", "local"]);
        let model = || Some("openrouter/free".to_owned());
        assert_eq!(chip_label(&both, Some("vm".into()), model()).as_deref(), Some("vm · openrouter/free"));
        assert_eq!(chip_label(&offered(&["local"]), Some("local".into()), model()).as_deref(), Some("openrouter/free"));
        assert_eq!(chip_label(&both, Some("local".into()), None).as_deref(), Some("local"));
        assert_eq!(chip_label(&[], None, None), None);
    }
}
