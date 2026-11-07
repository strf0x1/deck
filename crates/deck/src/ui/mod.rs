//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `chrome`, `panels`, `conversation` and `access_panel` render it; `board`, `rail`,
//! `session`, `access` and `spring` are pure state with their own tests; `theme` is
//! the visual vocabulary.

pub mod access;
mod access_grants;
mod access_panel;
mod attachment;
pub mod board;
pub mod boundary;
mod chrome;
mod conversation;
mod credentials;
mod panels;
pub mod publication;
mod publication_panel;
mod publishing;
pub mod rail;
pub mod root;
pub mod session;
pub mod spring;
pub mod tasks;
pub mod theme;
