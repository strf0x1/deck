//! A dev-only control channel: with `DECK_CONTROL=<path>` set, deck listens
//! on a Unix socket there and lets an agent drive and inspect the live window
//! (type, press keys, click, drag, read state) without keyboard focus.
//! `protocol` is the line format, `socket` the listener threads, `drive`
//! carries commands out on the window.
//!
//! The design choice: the socket threads never touch the UI; requests cross
//! to it on a channel the window's foreground task polls, the same pattern as
//! the ACP client, and run one at a time, in order.

pub mod drive;
pub mod protocol;
pub mod socket;

pub use socket::Request;

use crate::ui::root::Deck;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{App, WindowHandle};
use std::path::Path;

/// The environment variable naming the socket path.
pub const ENV: &str = "DECK_CONTROL";

/// Listen on `path` and run its commands against `window` until it closes.
pub fn serve(path: &Path, window: WindowHandle<Deck>, cx: &mut App) -> anyhow::Result<()> {
    pump(socket::listen(path)?, window, cx);
    Ok(())
}

/// Run requests from `rx` against `window`, one at a time, in order.
pub fn pump(mut rx: UnboundedReceiver<Request>, window: WindowHandle<Deck>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Some(req) = rx.next().await {
            let reply = drive::run(req.cmd, window, cx).await;
            let _ = req.reply.send(reply);
        }
    })
    .detach();
}
