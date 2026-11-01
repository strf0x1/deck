//! Deck's GPUI views. `root` is the window's view and owns all state;
//! `panels` renders it; `board`, `session` and `spring` are pure state with
//! their own tests; `shape` and `theme` are the visual vocabulary.

pub mod board;
mod panels;
pub mod root;
pub mod session;
pub mod shape;
pub mod spring;
pub mod theme;
