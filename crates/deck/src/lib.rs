//! Deck: a native ACP client for directing agents, starting with construct.
//! `ui` is the GPUI window; `app` opens it; `control` is the dev-only socket
//! that drives the window. The protocol itself is the `deck_acp` crate, which
//! never depends on GPUI, so protocol work builds and tests without it.

pub mod app;
#[cfg(unix)]
pub mod control;
pub mod ui;
