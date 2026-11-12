//! Deck's colours, type and icons, as named tokens. Every view takes colours
//! from here, never from a literal, so the look can be retuned in one file.
//!
//! The design choice: charcoal glass with colour spent sparingly, and every
//! hue means one thing. Surfaces are neutral translucent layers over a
//! near-black base, so depth comes from stacking. A coloured mark keeps a
//! solid dark body and puts its hue in its text and border (`well`), so it
//! pops off the glass without a wash of colour. Purple is the user (their
//! turns, focus, the primary action), light teal is the agent at work, warm
//! amber waits on a human, green is done or allowed, red failed or is denied.
//! Tool kinds: looking is steel blue, changing files lavender, running code
//! teal, leaving the machine orange, deleting red.

use super::highlight::Class;
use gpui::{Background, BoxShadow, Hsla, Rgba, linear_color_stop, linear_gradient, point, px, rgba};

/// Interface text.
pub const SANS: &str = "Adwaita Sans";
/// Code, tool calls, and Nerd Font icons.
pub const MONO: &str = "JetBrainsMono Nerd Font";
/// Icons never inherit a selected output font or its masking features.
pub const ICONS: &str = "JetBrainsMono Nerd Font";

/// Nerd Font (Font Awesome range) glyphs; render with [`MONO`].
pub mod icon {
    pub const SESSIONS: &str = "\u{f009}";
    pub const PROJECT: &str = "\u{f07b}";
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
    pub const CHEVRON_DOWN: &str = "\u{f078}";
    pub const CHEVRON_RIGHT: &str = "\u{f054}";
    pub const CUBE: &str = "\u{f1b2}";
    pub const BOLT: &str = "\u{f0e7}";
    pub const KEY: &str = "\u{f084}";
    pub const LOCK: &str = "\u{f023}";
    pub const STOP: &str = "\u{f04d}";
    pub const REFRESH: &str = "\u{f021}";
    pub const WARN: &str = "\u{f071}";
    pub const QUESTION: &str = "\u{f128}";
    pub const EYE: &str = "\u{f06e}";
    pub const DOT: &str = "\u{f111}";
    pub const RING: &str = "\u{f10c}";
    pub const GITHUB: &str = "\u{f09b}";
    pub const EXTERNAL: &str = "\u{f08e}";
    pub const CLOSE: &str = "\u{f00d}";
    pub const USER: &str = "\u{f007}";
    pub const SERVER: &str = "\u{f233}";
    pub const ARCHIVE: &str = "\u{f187}";
    pub const GAUGE: &str = "\u{f0e4}";
    pub const GEAR: &str = "\u{f013}";
}

/// Text sizes by role. Six steps: views pick a role, never a number.
pub mod size {
    /// Key hints, language labels, counters on chips.
    pub const MICRO: f32 = 11.;
    /// Mono detail: tool subjects, diffs, output, the status row.
    pub const SMALL: f32 = 13.;
    /// Interface labels: buttons, card kinds, pills.
    pub const LABEL: f32 = 14.;
    /// Reading text: prose, the prompt, the transcript.
    pub const BODY: f32 = 15.;
    /// Pane titles and second-level headings.
    pub const TITLE: f32 = 17.;
    /// The empty state and first-level headings.
    pub const DISPLAY: f32 = 21.;

    /// Role sizes scale together from a 14px rem base set by the window.
    pub fn scaled(pixels: f32) -> gpui::Rems {
        gpui::rems(pixels / 14.)
    }
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
    /// The whole window: translucent charcoal so the compositor's blur shows through.
    pub fn base() -> Background {
        linear_gradient(160., linear_color_stop(rgba(0x17171ae8), 0.), linear_color_stop(rgba(0x0c0c0ef4), 1.))
    }
    /// A pane: one layer of neutral glass.
    pub fn panel() -> Background {
        linear_gradient(180., linear_color_stop(rgba(0xffffff0c), 0.), linear_color_stop(rgba(0xffffff05), 1.))
    }
    /// Something sitting on a pane: a card, a field, a bubble.
    pub fn raised() -> Rgba {
        rgba(0xffffff0a)
    }
    pub fn raised_hover() -> Rgba {
        rgba(0xffffff14)
    }
    /// A well cut into a card: command text, output, code.
    pub fn sunken() -> Rgba {
        rgba(0x0000004d)
    }
    /// The solid dark body of a coloured mark (a button, a decision card,
    /// a chip): its colour lives in its text and border, never its fill, so
    /// it reads as darker than the glass around it.
    pub fn well() -> Rgba {
        rgba(0x0e0e11f2)
    }
    /// A surface that floats over others (the expanded rail): mostly opaque,
    /// since GPUI cannot blur what is behind an element.
    pub fn float() -> Rgba {
        rgba(0x18181bff)
    }
    pub fn edge() -> Rgba {
        rgba(0xffffff14)
    }
    pub fn edge_strong() -> Rgba {
        rgba(0xffffff2a)
    }
    /// A scrollbar thumb: neutral glass, brighter while hovered or held.
    pub fn thumb() -> Rgba {
        rgba(0xffffff33)
    }
    pub fn thumb_active() -> Rgba {
        rgba(0xffffff66)
    }
    pub fn shadow() -> Hsla {
        rgba(0x00000088).into()
    }
    pub fn text() -> Rgba {
        rgba(0xececf0ff)
    }
    pub fn text_dim() -> Rgba {
        rgba(0x9d9da8ff)
    }
    pub fn text_faint() -> Rgba {
        rgba(0x60606bff)
    }
    /// Text on a bright fill (a count badge).
    pub fn ink() -> Rgba {
        rgba(0x111114ff)
    }
    /// The user: their turns, focus, selection, the primary action.
    pub fn accent() -> Hsla {
        rgba(0xa38bffff).into()
    }
    /// Emphasis in agent prose (markdown bold): the accent, lighter.
    pub fn emphasis() -> Hsla {
        rgba(0xcbbdffff).into()
    }
    /// An agent doing work; running code. A light teal.
    pub fn cyan() -> Hsla {
        rgba(0x6fdccbff).into()
    }
    /// Decisions waiting on a human. Warm, so it is spent on borders and
    /// text, not fills.
    pub fn amber() -> Hsla {
        rgba(0xf5a65bff).into()
    }
    /// Done, allowed, added.
    pub fn lime() -> Hsla {
        rgba(0x9fd67aff).into()
    }
    /// Failed, denied, removed.
    pub fn red() -> Hsla {
        rgba(0xf07a7aff).into()
    }
    /// Changing files; the sealed VM. A pinker lavender than the accent.
    pub fn violet() -> Hsla {
        rgba(0xd29cf0ff).into()
    }
    /// Looking: reads and searches. A quiet steel blue.
    pub fn sky() -> Hsla {
        rgba(0x8db4dcff).into()
    }
    /// Leaving the machine: the network.
    pub fn orange() -> Hsla {
        rgba(0xf08c5aff).into()
    }
    /// A stable hue for a name, so a project keeps its colour everywhere.
    pub fn hue(name: &str) -> Hsla {
        let hues = [Self::accent(), Self::cyan(), Self::amber(), Self::lime(), Self::sky(), Self::violet()];
        let sum = name.bytes().fold(0usize, |sum, byte| sum.wrapping_mul(31).wrapping_add(usize::from(byte)));
        hues[sum % hues.len()]
    }
    /// A faint accent wash, for the user's own turns and the pane's top edge.
    pub fn sweep(a: f32) -> Background {
        linear_gradient(
            90.,
            linear_color_stop(tint(Self::accent(), a), 0.),
            linear_color_stop(tint(Self::accent(), a * 0.4), 1.),
        )
    }
    /// Code: the accents again, a little softer, plus one blue for types,
    /// so highlighted code sits in the same palette as the window.
    pub fn code(class: Class) -> Hsla {
        match class {
            Class::Comment => Self::text_faint().into(),
            Class::String => rgba(0xb0d98ee6).into(),
            Class::Keyword => rgba(0xb4a3ffff).into(),
            Class::Number | Class::Constant => rgba(0xf2b880ff).into(),
            Class::Function => rgba(0x7fdccdff).into(),
            Class::Type => rgba(0x9cc0e6ff).into(),
        }
    }
}

/// `color` at alpha `a`, for tints and washes of an accent.
pub fn tint(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}

/// A soft halo in `color`: what marks the one live thing in a region. Kept
/// faint, so a lit mark reads as lit without turning into a neon sign.
pub fn glow(color: Hsla, strength: f32) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: tint(color, 0.22 * strength),
        offset: point(px(0.), px(0.)),
        blur_radius: px(10.),
        spread_radius: px(0.),
    }]
}

#[cfg(test)]
mod tests {
    use super::Tokens;

    #[test]
    fn a_name_keeps_its_hue_and_names_spread_over_several() {
        assert_eq!(Tokens::hue("deck"), Tokens::hue("deck"));
        let hues: Vec<_> = ["deck", "construct", "qwen-engine", "alpha", "dotfiles"].map(Tokens::hue).to_vec();
        assert!(hues.iter().any(|hue| *hue != hues[0]), "every name landed on one hue");
    }
}
