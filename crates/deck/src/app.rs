//! Opening the Deck window: GPUI's platform application, one window with a
//! blurred translucent background, and the agent launched into it.
//!
//! The design choice: `app_id` is "deck" so the compositor can be told how to
//! treat it (on Hyprland, a window rule for blur and opacity); GPUI asks for
//! blur but the compositor decides.

use crate::ui::root::Deck;
use gpui::{
    App, AppContext as _, Bounds, Focusable as _, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use gpui_platform::application;
use std::path::PathBuf;

pub fn run(argv: Vec<String>, cwd: PathBuf) {
    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_background: WindowBackgroundAppearance::Blurred,
            app_id: Some("deck".into()),
            window_min_size: Some(size(px(760.), px(480.))),
            ..Default::default()
        };
        let opened = cx.open_window(opts, |window, cx| {
            let deck = cx.new(|cx| Deck::launch(argv, cwd, cx));
            deck.focus_handle(cx).focus(window, cx);
            deck
        });
        match opened {
            Ok(window) => control(window, cx),
            Err(e) => {
                eprintln!("deck: could not open a window: {e:#}");
                cx.quit();
            }
        }
        cx.activate(true);
    });
}

/// Start the control socket when `DECK_CONTROL` names a path.
#[cfg(unix)]
fn control(window: WindowHandle<Deck>, cx: &mut App) {
    let Some(path) = std::env::var_os(crate::control::ENV) else { return };
    let path = PathBuf::from(path);
    match crate::control::serve(&path, window, cx) {
        Ok(()) => eprintln!("deck: control socket at {}", path.display()),
        Err(e) => eprintln!("deck: no control socket: {e:#}"),
    }
}

#[cfg(not(unix))]
fn control(_: WindowHandle<Deck>, _: &mut App) {}
