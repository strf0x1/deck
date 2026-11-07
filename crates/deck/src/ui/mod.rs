//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `chrome`, `panels`, `conversation`, `transcript_list`, `cards`, `turn_footer`, `prose`,
//! `prompt` and `access_panel` render it, `composer` is the text field's
//! element and `input` its keys and IME; `board`, `rail`, `session`,
//! `transcript`, `markdown`, `diff`, `editor`, `access` and `spring` are pure
//! state with their own tests; `theme` is the visual vocabulary.

pub mod access;
mod access_grants;
mod access_panel;
mod attachment;
pub mod board;
pub mod boundary;
mod cards;
mod chrome;
mod composer;
mod conversation;
mod copy;
mod credentials;
pub mod diff;
pub mod editor;
mod input;
pub mod markdown;
mod panels;
mod prompt;
mod prose;
pub mod publication;
mod publication_panel;
mod publishing;
pub mod rail;
pub mod root;
pub mod session;
pub mod spring;
pub mod tasks;
pub mod theme;
pub mod transcript;
mod transcript_list;
mod turn_footer;
