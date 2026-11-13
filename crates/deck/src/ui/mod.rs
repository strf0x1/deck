//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `chrome`, `panels`, `conversation`, `project_view`, `sidebar_view`, `transcript_list`, `cards`, `card_body`, `folds`,
//! `turn_footer`, `prose`, `prompt`, `access_panel`, `settings_panel`, `settings_form` and `model_menu` render it, `composer` is the text field's
//! element and `input` its keys and IME; `glyph` is the icon element; `attached` is one task's connection
//! state, `board`, `rail`, `session`, `transcript`, `markdown`, `diff`,
//! `highlight`, `motion`, `editor`, `access`, `settings`, `settings_rows`, `new_session`, `projects`,
//! `sidebar` and `spring` are pure state with their own tests; `header_title`
//! is the header's text, `titles` records a task's title with the daemon;
//! `theme` is the visual vocabulary.

pub mod access;
mod access_grants;
mod access_panel;
mod appearance;
mod appearance_ops;
mod attached;
mod attachment;
pub mod attention;
mod attention_state;
pub mod board;
pub mod boundary;
mod card_body;
mod cards;
mod chrome;
mod composer;
mod conversation;
mod copy;
mod credentials;
pub mod diff;
pub mod editor;
mod folds;
pub mod fonts;
mod glyph;
mod header_title;
pub mod highlight;
mod input;
pub mod markdown;
mod model_menu;
mod model_search;
pub mod motion;
pub mod new_session;
mod panels;
mod project_view;
mod projects;
mod prompt;
mod prose;
pub mod publication;
mod publication_panel;
mod publishing;
pub mod rail;
pub mod root;
pub mod scrollbar;
mod scrollbar_element;
pub mod session;
pub mod settings;
mod settings_fields;
mod settings_form;
mod settings_ops;
mod settings_panel;
mod settings_rows;
pub mod sidebar;
mod sidebar_narrow;
mod sidebar_rows;
mod sidebar_state;
mod sidebar_tasks;
pub mod sidebar_view;
mod specimens;
pub mod spring;
pub mod tasks;
pub mod theme;
mod titles;
pub mod transcript;
mod transcript_list;
mod turn_footer;
pub mod typography;
