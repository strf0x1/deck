//! The backends one runtime drives, by the name task records carry
//! (ADR 0016). A record is only ever run by the backend whose name it
//! carries; a record naming a backend this runtime does not have is left
//! alone, as before.
//!
//! The design choice: the first backend is the default, so a caller that
//! names none gets what the daemon was configured for, and a caller that
//! names one this daemon lacks is refused rather than quietly given another.

use crate::backend::Backend;
use anyhow::{Result, anyhow, bail};

pub(super) struct Backends(Vec<Box<dyn Backend>>);

impl Backends {
    pub(super) fn new(backends: Vec<Box<dyn Backend>>) -> Result<Backends> {
        let Some(first) = backends.first() else { bail!("a runtime needs a backend") };
        let mut names = vec![first.name()];
        for backend in &backends[1..] {
            if names.contains(&backend.name()) {
                bail!("two backends are both named {}", backend.name());
            }
            names.push(backend.name());
        }
        Ok(Backends(backends))
    }

    /// The default backend's name.
    pub(super) fn default_name(&self) -> &'static str {
        self.0[0].name()
    }

    /// Every backend's name, the default first.
    pub(super) fn names(&self) -> Vec<&'static str> {
        self.0.iter().map(|backend| backend.name()).collect()
    }

    pub(super) fn has(&self, name: &str) -> bool {
        self.0.iter().any(|backend| backend.name() == name)
    }

    /// The backend a new task asked for, or the default when it asked for none.
    pub(super) fn chosen(&mut self, requested: Option<&str>) -> Result<&mut Box<dyn Backend>> {
        let name = requested.unwrap_or(self.default_name()).to_owned();
        let offered = self.names().join(", ");
        self.get(&name).map_err(|_| anyhow!("this deckd has no {name} backend (it offers {offered})"))
    }

    /// The backend a record names.
    pub(super) fn get(&mut self, name: &str) -> Result<&mut Box<dyn Backend>> {
        self.0.iter_mut().find(|backend| backend.name() == name).ok_or_else(|| anyhow!("no {name} backend here"))
    }

    /// The backend a task's record names, or why this runtime will not run it.
    pub(super) fn of(&mut self, task: &crate::Task) -> Result<&mut Box<dyn Backend>> {
        let offered = self.names().join(", ");
        self.get(&task.backend).map_err(|_| {
            anyhow!("task {} belongs to the {} backend, which this deckd lacks ({offered})", task.id, task.backend)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::Local;

    struct Named(&'static str);
    impl Backend for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn start(
            &mut self,
            _: &crate::Task,
            _: crate::record::RecordLock,
            _: crate::Exited,
        ) -> Result<Box<dyn crate::Instance>> {
            bail!("not started in this test")
        }
        fn destroy(&mut self, _: &crate::Task) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_first_is_the_default_and_an_unknown_name_is_refused() {
        let mut backends = Backends::new(vec![Box::new(Named("vm")), Box::new(Local)]).unwrap();
        assert_eq!(backends.default_name(), "vm");
        assert_eq!(backends.names(), ["vm", "local"]);
        assert_eq!(backends.chosen(None).unwrap().name(), "vm");
        assert_eq!(backends.chosen(Some("local")).unwrap().name(), "local");
        let refused = backends.chosen(Some("cloud")).err().unwrap().to_string();
        assert!(refused.contains("no cloud backend") && refused.contains("vm, local"), "{refused}");
    }

    #[test]
    fn a_task_runs_on_the_backend_it_asked_for_and_the_record_says_which() {
        let dir = std::env::temp_dir().join(format!("deck-runtime-backends-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = |backend: Option<&str>| crate::Spec {
            argv: vec!["agent".into()],
            cwd: dir.clone(),
            project: Some("git:test".into()),
            backend: backend.map(str::to_owned),
            ..Default::default()
        };
        let both: Vec<Box<dyn Backend>> = vec![Box::new(Named("vm")), Box::new(Local)];
        let rt = super::super::Runtime::open_with(&dir, both).unwrap();
        assert_eq!(rt.backends(), ["vm", "local"]);

        let default = rt.create(spec(None)).unwrap();
        let local = rt.create(spec(Some("local"))).unwrap();
        assert_eq!(rt.task(&default).unwrap().backend, "vm", "none named is the default");
        let local = rt.task(&local).unwrap();
        assert_eq!(local.backend, "local");
        assert_eq!(local.spec.backend, None, "the record's own backend says where it runs");
        assert!(rt.create(spec(Some("cloud"))).is_err(), "a backend this deckd lacks is refused");
        drop(rt);

        // Reopened without the VM: its record is left alone, the local one
        // still belongs here.
        let rt = super::super::Runtime::open(&dir, Local).unwrap();
        assert!(rt.destroy(&default).is_err());
        rt.destroy(&local.id).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn none_or_two_of_one_name_is_refused() {
        assert!(Backends::new(vec![]).is_err());
        assert!(Backends::new(vec![Box::new(Local), Box::new(Local)]).is_err());
    }
}
