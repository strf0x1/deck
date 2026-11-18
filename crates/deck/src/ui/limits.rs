//! The coordinator's Limits panel as plain state (ADR 0011 §4, §9): its
//! caps, the worker and model budgets, and the project's coordinator
//! command, as `coordinator_policy` reads and writes them.
//!
//! The design choice: each field is a composer [`Editor`], so typing and IME
//! work as in the prompt, and a value is checked with the runtime's own
//! `Policy::validate` before anything is sent. The panel keeps the settings
//! it loaded, so a save can tell that someone changed them meanwhile and
//! reload instead of writing over that change. Only the window reaches it;
//! the coordinator has no operation that does.

use super::editor::Editor;
use deck_runtime::coordinator::{Budget, Settings, default_argv};

/// A field of the panel, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Running,
    PerHour,
    WorkerRequests,
    WorkerTokens,
    ModelRequests,
    ModelTokens,
    Command,
}

impl Field {
    pub const ALL: [Field; 7] = [
        Field::Running,
        Field::PerHour,
        Field::WorkerRequests,
        Field::WorkerTokens,
        Field::ModelRequests,
        Field::ModelTokens,
        Field::Command,
    ];

    fn index(self) -> usize {
        Field::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// The field's label.
    pub fn label(self) -> &'static str {
        match self {
            Field::Running => "Running sessions at once",
            Field::PerHour => "New sessions per hour",
            Field::WorkerRequests => "Each session's model requests",
            Field::WorkerTokens => "Each session's model tokens",
            Field::ModelRequests => "The coordinator's model requests",
            Field::ModelTokens => "The coordinator's model tokens",
            Field::Command => "Coordinator command",
        }
    }

    /// The control socket's name for it.
    pub fn name(self) -> &'static str {
        match self {
            Field::Running => "running",
            Field::PerHour => "per_hour",
            Field::WorkerRequests => "worker_requests",
            Field::WorkerTokens => "worker_tokens",
            Field::ModelRequests => "model_requests",
            Field::ModelTokens => "model_tokens",
            Field::Command => "command",
        }
    }

    /// What an empty field means.
    pub fn empty(self) -> &'static str {
        match self {
            Field::Running | Field::PerHour => "required",
            Field::Command => "construct acp",
            _ => "unlimited",
        }
    }
}

/// The open panel: whose limits, what was loaded, the fields being edited.
#[derive(Default)]
pub struct Limits {
    /// The coordinator whose policy is open; `None` while closed.
    coordinator: Option<String>,
    loaded: Option<Settings>,
    editors: [Editor; 7],
    focus: Option<Field>,
    error: Option<String>,
    notice: Option<String>,
    busy: bool,
}

impl Limits {
    pub fn is_open(&self) -> bool {
        self.coordinator.is_some()
    }

    pub fn coordinator(&self) -> Option<&str> {
        self.coordinator.as_deref()
    }

    /// Open for `coordinator` with the caret in `field`, or the first field;
    /// the fields fill when its settings arrive.
    pub fn open(&mut self, coordinator: &str, field: Option<Field>) {
        *self = Limits { coordinator: Some(coordinator.to_string()), busy: true, ..Limits::default() };
        self.focus = Some(field.unwrap_or(Field::Running));
    }

    pub fn close(&mut self) {
        *self = Limits::default();
    }

    /// Fill the fields from `settings` as `deckd` holds them.
    pub fn loaded(&mut self, settings: Settings) {
        let number = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
        let p = &settings.policy;
        let values = [
            p.max_concurrent.to_string(),
            p.creates_per_hour.to_string(),
            number(p.worker_budget.requests),
            number(p.worker_budget.tokens),
            number(p.model_budget.requests),
            number(p.model_budget.tokens),
            settings.coordinator_argv.as_ref().map(|argv| argv.join(" ")).unwrap_or_default(),
        ];
        for (editor, value) in self.editors.iter_mut().zip(values) {
            *editor = Editor::new();
            editor.insert(&value);
        }
        self.loaded = Some(settings);
        self.busy = false;
    }

    /// The settings the panel loaded, which a save compares with `deckd`'s.
    pub fn base(&self) -> Option<&Settings> {
        self.loaded.as_ref()
    }

    pub fn value(&self, field: Field) -> &str {
        self.editors[field.index()].text()
    }

    pub fn focused(&self) -> Option<Field> {
        self.focus.filter(|_| self.loaded.is_some())
    }

    pub fn focus(&mut self, field: Field) {
        self.focus = Some(field);
    }

    /// Move the caret to the next field, or the previous with `back`.
    pub fn next_field(&mut self, back: bool) {
        let at = self.focus.map_or(0, Field::index);
        let n = Field::ALL.len();
        self.focus = Some(Field::ALL[if back { (at + n - 1) % n } else { (at + 1) % n }]);
    }

    pub fn editor(&self) -> Option<&Editor> {
        self.focused().map(|field| &self.editors[field.index()])
    }

    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        let field = self.focused()?;
        Some(&mut self.editors[field.index()])
    }

    /// The fields as settings, checked as `deckd` will check them; the error
    /// says which field and why.
    pub fn read(&self) -> Result<Settings, String> {
        let base = self.loaded.as_ref().ok_or("the limits are still loading")?;
        let cap = |field: Field| -> Result<u32, String> {
            self.value(field).trim().parse::<u32>().map_err(|_| format!("{}: a whole number", field.label()))
        };
        let budget = |field: Field| -> Result<Option<u64>, String> {
            match self.value(field).trim() {
                "" => Ok(None),
                text => {
                    text.parse::<u64>().map(Some).map_err(|_| format!("{}: a whole number, or empty", field.label()))
                }
            }
        };
        let mut policy = base.policy.clone();
        policy.max_concurrent = cap(Field::Running)?;
        policy.creates_per_hour = cap(Field::PerHour)?;
        policy.worker_budget =
            Budget { requests: budget(Field::WorkerRequests)?, tokens: budget(Field::WorkerTokens)? };
        policy.model_budget = Budget { requests: budget(Field::ModelRequests)?, tokens: budget(Field::ModelTokens)? };
        policy.validate().map_err(|e| format!("{e:#}"))?;
        let words: Vec<String> = self.value(Field::Command).split_whitespace().map(str::to_owned).collect();
        // The default command is recorded as none, so it follows the default.
        let coordinator_argv = (!words.is_empty() && words != default_argv()).then_some(words);
        Ok(Settings { policy, coordinator_argv })
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    /// A save is on its way.
    pub fn saving(&mut self) {
        self.busy = true;
        self.error = None;
        self.notice = None;
    }

    /// Why the values were refused, here or by `deckd`.
    pub fn refuse(&mut self, why: String) {
        self.busy = false;
        self.error = Some(why);
    }

    /// The save landed: the panel shows what `deckd` now holds.
    pub fn saved(&mut self, settings: Settings) {
        let command_changed = self.loaded.as_ref().map(|l| &l.coordinator_argv) != Some(&settings.coordinator_argv);
        self.loaded(settings);
        self.error = None;
        self.notice = Some(if command_changed {
            "Saved. The new command applies to the next coordinator.".into()
        } else {
            "Saved.".into()
        });
    }

    /// Someone changed the limits since they were loaded: show theirs and
    /// let the user decide again, rather than writing over them.
    pub fn stale(&mut self, current: Settings) {
        self.loaded(current);
        self.error = None;
        self.notice = Some("These limits changed since you opened them; they are reloaded. Save again.".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_runtime::coordinator::Policy;

    fn settings() -> Settings {
        Settings { policy: Policy::new(vec!["git:a".into()]).unwrap(), coordinator_argv: None }
    }

    fn set(limits: &mut Limits, field: Field, text: &str) {
        limits.focus(field);
        let editor = limits.editor_mut().unwrap();
        editor.select_all();
        editor.insert(text);
    }

    #[test]
    fn the_fields_read_back_as_the_policy_with_empty_budgets_unlimited() {
        let mut limits = Limits::default();
        limits.open("c1", Some(Field::PerHour));
        assert!(limits.busy() && limits.focused().is_none(), "nothing to type in before the load");
        limits.loaded(settings());
        assert_eq!(limits.focused(), Some(Field::PerHour), "Raise puts the caret at its cap");
        assert_eq!(limits.value(Field::Running), "4");
        assert_eq!(limits.read().unwrap(), settings(), "an untouched panel saves what it loaded");
        set(&mut limits, Field::Running, "6");
        set(&mut limits, Field::WorkerTokens, "200000");
        set(&mut limits, Field::Command, "  construct   acp  --fast ");
        let read = limits.read().unwrap();
        assert_eq!(read.policy.max_concurrent, 6);
        assert_eq!(read.policy.worker_budget, Budget { requests: None, tokens: Some(200_000) });
        assert_eq!(read.coordinator_argv, Some(vec!["construct".into(), "acp".into(), "--fast".into()]));
        set(&mut limits, Field::Command, "construct acp");
        assert_eq!(limits.read().unwrap().coordinator_argv, None, "the default is recorded as none");
    }

    #[test]
    fn an_invalid_value_is_refused_with_its_field_before_anything_is_sent() {
        let mut limits = Limits::default();
        limits.open("c1", None);
        limits.loaded(settings());
        set(&mut limits, Field::Running, "four");
        assert!(limits.read().unwrap_err().contains("Running sessions at once"));
        set(&mut limits, Field::Running, "0");
        assert!(limits.read().unwrap_err().contains("max_concurrent"), "the runtime's own bound");
        set(&mut limits, Field::Running, "4");
        set(&mut limits, Field::ModelRequests, "0");
        assert!(limits.read().is_err(), "a zero budget is no budget");
    }

    #[test]
    fn tab_walks_the_fields_and_a_stale_save_reloads_theirs() {
        let mut limits = Limits::default();
        limits.open("c1", Some(Field::Command));
        limits.loaded(settings());
        limits.next_field(false);
        assert_eq!(limits.focused(), Some(Field::Running), "tab wraps");
        limits.next_field(true);
        assert_eq!(limits.focused(), Some(Field::Command));
        let mut theirs = settings();
        theirs.policy.max_concurrent = 9;
        limits.saving();
        limits.stale(theirs.clone());
        assert_eq!(limits.value(Field::Running), "9");
        assert_eq!(limits.base(), Some(&theirs));
        assert!(limits.notice().unwrap().contains("Save again"));
        limits.saved(Settings { coordinator_argv: Some(vec!["x".into()]), ..theirs });
        assert!(limits.notice().unwrap().contains("next coordinator"));
        limits.close();
        assert!(!limits.is_open());
    }
}
