//! Deck: a native ACP client for directing agents, starting with construct.
//! `ui` is the GPUI window; `app` opens it; `control` is the dev-only socket
//! that drives the window; `task` runs the window's agent through
//! `deck_runtime`. The protocol (`deck_acp`) and the runtime never depend on
//! GPUI, so that work builds and tests without it.

pub mod app;
#[cfg(unix)]
pub mod control;
pub mod task;
pub mod ui;
