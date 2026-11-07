//! Deck's colours, type and icons, as named tokens. Every view takes colours
//! from here, never from a literal, so the look can be retuned in one file.
//!
//! The design choice: dark neutral glass, not blue. Surfaces are translucent
//! whites layered over a near-black base, so depth comes from stacking, and
//! colour is spent only where the eye should land: a state dot, the active
//! rail marker, the selected session, the send button. The accents themselves
//! are vivid; their scarcity is what keeps them loud.

use super::highlight::Class;
use gpui::{Background, Hsla, Rgba, linear_color_stop, linear_gradient, rgba};

/// Interface text.
pub const SANS: &str = "Adwaita Sans";
/// Code, tool calls, and Nerd Font icons.
pub const MONO: &str = "JetBrainsMono Nerd Font";

/// Nerd Font (Font Awesome range) glyphs; render with [`MONO`].
pub mod icon {
    pub const SESSIONS: &str = "\u{f009}";
    pub const AGENTS: &str = "\u{f1b3}";
    pub const FILES: &str = "\u{f114}";
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
    pub const EDIT: &str = "\u{f040}";
    pub const TRASH: &str = "\u{f1f8}";
    pub const MOVE: &str = "\u{f0ec}";
    pub const GLOBE: &str = "\u{f0ac}";
    pub const WRENCH: &str = "\u{f0ad}";
    pub const BULB: &str = "\u{f0eb}";
    pub const DOWN: &str = "\u{f063}";
    pub const COPY: &str = "\u{f0c5}";
}

/// Text sizes by role. Six steps: views pick a role, never a number.
pub mod size {
    /// Key hints, language labels, counters on chips.
    pub const MICRO: f32 = 10.5;
    /// Mono detail: tool subjects, diffs, output, the status row.
    pub const SMALL: f32 = 11.5;
    /// Interface labels: buttons, card kinds, pills.
    pub const LABEL: f32 = 12.5;
    /// Reading text: prose, the prompt, the transcript.
    pub const BODY: f32 = 13.5;
    /// Pane titles and second-level headings.
    pub const TITLE: f32 = 15.;
    /// The empty state and first-level headings.
    pub const DISPLAY: f32 = 17.;
}

/// Corner radii by role, from the smallest mark to a pane.
pub mod radius {
    pub const MARK: f32 = 2.;
    pub const CHIP: f32 = 6.;
    pub const CONTROL: f32 = 10.;
    pub const CARD: f32 = 12.;
    pub const BUBBLE: f32 = 14.;
    pub const FIELD: f32 = 16.;
    pub const PANE: f32 = 18.;
}

/// Spacing by role for every pane, so density is tuned here.
pub mod space {
    /// Between transcript rows and pane sections.
    pub const ROW: f32 = 14.;
    /// The transcript's side margins.
    pub const MARGIN: f32 = 22.;
    /// Inside a card or a field.
    pub const INSET: f32 = 12.;
    /// Between items on one line.
    pub const TIGHT: f32 = 6.;
    /// A card header's height.
    pub const HEADER: f32 = 34.;
    /// A pane's side padding.
    pub const PANE: f32 = 22.;
    /// A pane's top and bottom padding.
    pub const PANE_V: f32 = 18.;
    /// Between stacked items in a pane.
    pub const STACK: f32 = 10.;
    /// Between items in a row.
    pub const LINE: f32 = 8.;
    /// Between sections in a pane.
    pub const SECTION: f32 = 12.;
    /// Inside a card or a control.
    pub const PAD: f32 = 14.;
    /// A small separation or padding.
    pub const DETAIL: f32 = 4.;
    /// A hairline gap or padding.
    pub const HAIR: f32 = 2.;
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
    /// Code: the accents again, a little softer, plus one blue for types,
    /// so highlighted code sits in the same palette as the window.
    pub fn code(class: Class) -> Hsla {
        match class {
            Class::Comment => Self::text_faint().into(),
            Class::String => rgba(0xb4e36ae6).into(),
            Class::Keyword => rgba(0xb0a3ffff).into(),
            Class::Number | Class::Constant => rgba(0xffc46eff).into(),
            Class::Function => rgba(0x6fe3cdff).into(),
            Class::Type => rgba(0x8ec5ffff).into(),
        }
    }
}

/// `color` at alpha `a`, for tints and washes of an accent.
pub fn tint(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}
