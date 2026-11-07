//! Deck's colours, type and icons, as named tokens. Every view takes colours
//! from here, never from a literal, so the look can be retuned in one file.
//!
//! The design choice: dark neutral glass, not blue. Surfaces are translucent
//! whites layered over a near-black base, so depth comes from stacking, and
//! colour is spent only where the eye should land: a state dot, the active
//! rail marker, the selected session, the send button. The accents themselves
//! are vivid; their scarcity is what keeps them loud.

use gpui::{Background, Hsla, Rgba, linear_color_stop, linear_gradient, rgba};

/// Interface text.
pub const SANS: &str = "Adwaita Sans";
/// Code, tool calls, and Nerd Font icons.
pub const MONO: &str = "JetBrainsMono Nerd Font";

/// Nerd Font (Font Awesome range) glyphs; render with [`MONO`].
pub mod icon {
    pub const SESSIONS: &str = "\u{f009}";
    pub const AGENTS: &str = "\u{f1b3}";
    pub const FLOWS: &str = "\u{f0e8}";
    pub const FILES: &str = "\u{f114}";
    pub const HISTORY: &str = "\u{f1da}";
    pub const SETTINGS: &str = "\u{f013}";
    pub const SEARCH: &str = "\u{f002}";
    pub const PLUS: &str = "\u{f067}";
    pub const BELL: &str = "\u{f0a2}";
    pub const SEND: &str = "\u{f1d8}";
    pub const TERMINAL: &str = "\u{f120}";
    pub const CHECK: &str = "\u{f00c}";
    pub const CROSS: &str = "\u{f00d}";
    pub const SPIN: &str = "\u{f1ce}";
    pub const SHIELD: &str = "\u{f132}";
    pub const PIN: &str = "\u{f08d}";
    pub const BRANCH: &str = "\u{f126}";
}

pub struct Tokens;

impl Tokens {
    /// The whole window: translucent near-black so the compositor's blur shows through.
    pub fn base() -> Background {
        linear_gradient(160., linear_color_stop(rgba(0x141416e6), 0.), linear_color_stop(rgba(0x0a0a0cf2), 1.))
    }
    /// A pane: one layer of glass.
    pub fn panel() -> Background {
        linear_gradient(180., linear_color_stop(rgba(0xffffff0d), 0.), linear_color_stop(rgba(0xffffff06), 1.))
    }
    /// Something sitting on a pane: a card, a field, a bubble.
    pub fn raised() -> Rgba {
        rgba(0xffffff0b)
    }
    pub fn raised_hover() -> Rgba {
        rgba(0xffffff14)
    }
    /// A surface that floats over others (the expanded rail): mostly opaque,
    /// since GPUI cannot blur what is behind an element.
    pub fn float() -> Rgba {
        rgba(0x19191cff)
    }
    pub fn edge() -> Rgba {
        rgba(0xffffff12)
    }
    pub fn edge_strong() -> Rgba {
        rgba(0xffffff26)
    }
    pub fn shadow() -> Hsla {
        rgba(0x00000080).into()
    }
    pub fn text() -> Rgba {
        rgba(0xededf0ff)
    }
    pub fn text_dim() -> Rgba {
        rgba(0x9b9ba5ff)
    }
    pub fn text_faint() -> Rgba {
        rgba(0x5d5d67ff)
    }
    /// Text on a light fill (the one solid button).
    pub fn ink() -> Rgba {
        rgba(0x111114ff)
    }
    /// The one signature accent: focus, selection, the user's voice.
    pub fn accent() -> Hsla {
        rgba(0x9d8cffff).into()
    }
    /// An agent doing work.
    pub fn mint() -> Hsla {
        rgba(0x5ee0c8ff).into()
    }
    /// Decisions waiting on a human.
    pub fn amber() -> Hsla {
        rgba(0xffb547ff).into()
    }
    /// Done, ready for review.
    pub fn lime() -> Hsla {
        rgba(0xb4e36aff).into()
    }
    pub fn red() -> Hsla {
        rgba(0xff6b6bff).into()
    }
}

/// `color` at alpha `a`, for tints and washes of an accent.
pub fn tint(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}
