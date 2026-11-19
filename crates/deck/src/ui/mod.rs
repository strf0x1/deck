//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `chrome`, `panels`, `conversation`, `project_view`, `sidebar_view`, `transcript_list`, `cards`, `card_body`, `folds`,
//! `turn_footer`, `prose`, `prompt`, `access_panel`, `settings_panel`, `settings_form` and `model_menu` render it, `composer` is the text field's
//! element and `input` its keys and IME; `glyph` is the icon element; `attached` is one task's connection
//! state, `board`, `rail`, `session`, `transcript`, `markdown`, `diff`,
//! `highlight`, `motion`, `sheen`, `band`, `exhale`, `marks`, `ripple`, `editor`, `selection`, `runs`, `access`, `settings`, `settings_rows`, `new_session`, `projects`,
//! `sidebar` and `spring` are pure state with their own tests; `header_title`
//! is the header's text, `titles` records a task's title with the daemon;
//! `selectable` draws a transcript run and `selecting` is its mouse and keys;
//! `coordinator`, `deck_tool`, `surfaced` and `limits` are a project
//! coordinator's pure state, drawn by `coordinator_header`, `deck_calls`,
//! `surfaced_view` and `limits_panel` and driven by `coordinator_ops`,
//! `limits_ops` and `notices`; `theme` is the visual vocabulary.

pub mod access;
mod access_grants;
mod access_panel;
mod appearance;
mod appearance_ops;
pub mod arrival;
mod attached;
mod attachment;
pub mod attention;
mod attention_state;
pub mod band;
pub mod board;
pub mod boundary;
mod card_body;
mod cards;
mod chrome;
mod composer;
mod conversation;
pub mod coordinator;
mod coordinator_header;
mod coordinator_ops;
mod copy;
mod credentials;
mod deck_calls;
pub mod deck_tool;
pub mod diff;
pub mod editor;
pub mod exhale;
mod folds;
pub mod fonts;
mod glyph;
mod header_title;
pub mod highlight;
mod input;
pub mod limits;
mod limits_ops;
mod limits_panel;
pub mod markdown;
pub mod marks;
mod model_menu;
mod model_search;
pub mod motion;
pub mod new_session;
mod notices;
mod panels;
mod project_view;
mod projects;
mod prompt;
mod prose;
pub mod publication;
mod publication_panel;
mod publishing;
pub mod rail;
pub mod ripple;
pub mod root;
mod runs;
pub mod scrollbar;
mod scrollbar_element;
mod selectable;
mod selecting;
pub mod selection;
pub mod session;
pub mod session_backend;
pub mod settings;
mod settings_fields;
mod settings_form;
mod settings_ops;
mod settings_panel;
mod settings_rows;
pub mod sheen;
pub mod sidebar;
mod sidebar_narrow;
mod sidebar_rows;
mod sidebar_state;
mod sidebar_tasks;
pub mod sidebar_view;
mod specimens;
pub mod spring;
pub mod surfaced;
mod surfaced_view;
pub mod tasks;
pub mod theme;
mod titles;
pub mod transcript;
mod transcript_list;
mod turn_footer;
pub mod typography;
mod worktree_view;
mod worktrees;
