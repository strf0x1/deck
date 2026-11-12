//! Application-local fonts and GPUI family-resolution diagnostics. Enumeration
//! alone is insufficient because GPUI's list includes uninstalled fallbacks.

use gpui::{App, font};
use std::borrow::Cow;

pub fn load(cx: &App) -> anyhow::Result<()> {
    cx.text_system().add_fonts(vec![
        Cow::Borrowed(include_bytes!("../../assets/fonts/JetBrainsMono.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/ObelMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../../assets/fonts/ObelMonoMasked-Regular.ttf")),
    ])
}

pub struct Resolution {
    pub requested: &'static str,
    pub resolved: String,
}

impl Resolution {
    pub fn exact(&self) -> bool {
        self.requested == self.resolved
    }

    pub fn label(&self) -> String {
        if self.exact() {
            format!("{} · available", self.requested)
        } else {
            format!("{} · fallback: {}", self.requested, self.resolved)
        }
    }
}

/// Report GPUI's selected family, not a concrete OS face or PostScript name.
pub fn resolve(family: &'static str, cx: &App) -> Resolution {
    let system = cx.text_system();
    let id = system.resolve_font(&font(family));
    let resolved = system.get_font_for_id(id).map_or_else(|| "unknown".into(), |font| font.family.to_string());
    Resolution { requested: family, resolved }
}
