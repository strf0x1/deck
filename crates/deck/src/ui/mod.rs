//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `chrome`, `panels` and `conversation` render it; `board`, `rail`,
//! `session` and `spring` are pure state with their own tests; `theme` is
//! the visual vocabulary.

mod attachment;
pub mod board;
mod chrome;
mod conversation;
mod panels;
pub mod rail;
pub mod root;
pub mod session;
pub mod spring;
pub mod theme;
