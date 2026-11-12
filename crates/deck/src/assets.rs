//! The window's embedded assets: the icon set the UI draws.
//!
//! The design choice: `include_bytes!` in a match, not an asset crate. The set
//! is closed and small, and a name the views use without a file behind it would
//! paint nothing at all and say nothing either, so
//! [`tests::every_named_icon_ships_bytes`] checks the two lists agree.

use crate::ui::theme::icon;
use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// The assets compiled into the binary; give this to `Application::with_assets`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/add.svg" => Some(include_bytes!("../assets/icons/add.svg")),
            "icons/arrow_downward.svg" => Some(include_bytes!("../assets/icons/arrow_downward.svg")),
            "icons/bolt.svg" => Some(include_bytes!("../assets/icons/bolt.svg")),
            "icons/build.svg" => Some(include_bytes!("../assets/icons/build.svg")),
            "icons/check.svg" => Some(include_bytes!("../assets/icons/check.svg")),
            "icons/circle.svg" => Some(include_bytes!("../assets/icons/circle.svg")),
            "icons/close.svg" => Some(include_bytes!("../assets/icons/close.svg")),
            "icons/commit.svg" => Some(include_bytes!("../assets/icons/commit.svg")),
            "icons/content_copy.svg" => Some(include_bytes!("../assets/icons/content_copy.svg")),
            "icons/delete.svg" => Some(include_bytes!("../assets/icons/delete.svg")),
            "icons/dns.svg" => Some(include_bytes!("../assets/icons/dns.svg")),
            "icons/edit.svg" => Some(include_bytes!("../assets/icons/edit.svg")),
            "icons/folder.svg" => Some(include_bytes!("../assets/icons/folder.svg")),
            "icons/folder_open.svg" => Some(include_bytes!("../assets/icons/folder_open.svg")),
            "icons/github.svg" => Some(include_bytes!("../assets/icons/github.svg")),
            "icons/grid_view.svg" => Some(include_bytes!("../assets/icons/grid_view.svg")),
            "icons/help.svg" => Some(include_bytes!("../assets/icons/help.svg")),
            "icons/inventory_2.svg" => Some(include_bytes!("../assets/icons/inventory_2.svg")),
            "icons/key.svg" => Some(include_bytes!("../assets/icons/key.svg")),
            "icons/keyboard_arrow_down.svg" => Some(include_bytes!("../assets/icons/keyboard_arrow_down.svg")),
            "icons/keyboard_arrow_right.svg" => Some(include_bytes!("../assets/icons/keyboard_arrow_right.svg")),
            "icons/language.svg" => Some(include_bytes!("../assets/icons/language.svg")),
            "icons/lens.svg" => Some(include_bytes!("../assets/icons/lens.svg")),
            "icons/lightbulb.svg" => Some(include_bytes!("../assets/icons/lightbulb.svg")),
            "icons/lock.svg" => Some(include_bytes!("../assets/icons/lock.svg")),
            "icons/notifications.svg" => Some(include_bytes!("../assets/icons/notifications.svg")),
            "icons/open_in_new.svg" => Some(include_bytes!("../assets/icons/open_in_new.svg")),
            "icons/person.svg" => Some(include_bytes!("../assets/icons/person.svg")),
            "icons/progress_activity.svg" => Some(include_bytes!("../assets/icons/progress_activity.svg")),
            "icons/push_pin.svg" => Some(include_bytes!("../assets/icons/push_pin.svg")),
            "icons/refresh.svg" => Some(include_bytes!("../assets/icons/refresh.svg")),
            "icons/search.svg" => Some(include_bytes!("../assets/icons/search.svg")),
            "icons/send.svg" => Some(include_bytes!("../assets/icons/send.svg")),
            "icons/settings.svg" => Some(include_bytes!("../assets/icons/settings.svg")),
            "icons/shield.svg" => Some(include_bytes!("../assets/icons/shield.svg")),
            "icons/smart_toy.svg" => Some(include_bytes!("../assets/icons/smart_toy.svg")),
            "icons/speed.svg" => Some(include_bytes!("../assets/icons/speed.svg")),
            "icons/stop.svg" => Some(include_bytes!("../assets/icons/stop.svg")),
            "icons/swap_horiz.svg" => Some(include_bytes!("../assets/icons/swap_horiz.svg")),
            "icons/terminal.svg" => Some(include_bytes!("../assets/icons/terminal.svg")),
            "icons/view_in_ar.svg" => Some(include_bytes!("../assets/icons/view_in_ar.svg")),
            "icons/visibility.svg" => Some(include_bytes!("../assets/icons/visibility.svg")),
            "icons/warning.svg" => Some(include_bytes!("../assets/icons/warning.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, prefix: &str) -> Result<Vec<SharedString>> {
        Ok(icon::ALL.iter().filter(|path| path.starts_with(prefix)).map(|path| (*path).into()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::Assets;
    use crate::ui::theme::icon;
    use gpui::AssetSource;

    /// A name in `theme::icon` with no file behind it paints no icon and logs
    /// nothing, so this is the only thing that says so.
    #[test]
    fn every_named_icon_ships_bytes() {
        for path in icon::ALL {
            let loaded = Assets.load(path).expect("reading an embedded asset");
            assert!(loaded.is_some_and(|bytes| !bytes.is_empty()), "{path} has no embedded bytes");
        }
    }
}
