//! Deck: a native ACP client for directing agents, starting with construct.
//! `acp` speaks the protocol to an agent subprocess; `ui` is the GPUI window;
//! `app` opens it; `control` is the dev-only socket that drives the window.

pub mod acp;
pub mod app;
#[cfg(unix)]
pub mod control;
pub mod ui;
