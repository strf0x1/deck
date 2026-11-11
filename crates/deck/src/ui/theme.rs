//! Deck's colours, type and icons, as named tokens. Every view takes colours
//! from here, never from a literal, so the look can be retuned in one file.
//!
//! The design choice: neon on deep ink, and every hue means one thing.
//! Surfaces are cool-tinted glass layered over a blue-black base, so depth
//! comes from stacking. Colour is never decoration: pink is the user (their
//! turns, focus, the primary action), cyan is the agent at work, amber waits
//! on a human, green is done or allowed, red failed or is denied. Tool kinds
//! run cool to warm by how far they reach: looking is blue, changing files
//! violet, running code cyan, leaving the machine orange, deleting red.

use super::highlight::Class;
use gpui::{Background, BoxShadow, Hsla, Rgba, linear_color_stop, linear_gradient, point, px, rgba};

/// Interface text.
pub const SANS: &str = "Adwaita Sans";
/// Code, tool calls, and Nerd Font icons.
pub const MONO: &str = "JetBrainsMono Nerd Font";

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
    /// The whole window: translucent blue-black so the compositor's blur shows through.
    pub fn base() -> Background {
        linear_gradient(160., linear_color_stop(rgba(0x0d1020e8), 0.), linear_color_stop(rgba(0x05060df4), 1.))
    }
    /// A pane: one layer of cool glass.
    pub fn panel() -> Background {
        linear_gradient(180., linear_color_stop(rgba(0xa9bcff10), 0.), linear_color_stop(rgba(0xa9bcff06), 1.))
    }
    /// Something sitting on a pane: a card, a field, a bubble.
    pub fn raised() -> Rgba {
        rgba(0xb4c4ff0d)
    }
    pub fn raised_hover() -> Rgba {
        rgba(0xb4c4ff1a)
    }
    /// A well cut into a card: command text, output, code.
    pub fn sunken() -> Rgba {
        rgba(0x03040a66)
    }
    /// A surface that floats over others (the expanded rail): mostly opaque,
    /// since GPUI cannot blur what is behind an element.
    pub fn float() -> Rgba {
        rgba(0x0f1220ff)
    }
    pub fn edge() -> Rgba {
        rgba(0xa9b8ff1c)
    }
    pub fn edge_strong() -> Rgba {
        rgba(0xa9b8ff3a)
    }
    pub fn shadow() -> Hsla {
        rgba(0x00000090).into()
    }
    pub fn text() -> Rgba {
        rgba(0xeef0fbff)
    }
    pub fn text_dim() -> Rgba {
        rgba(0xa0a7c4ff)
    }
    pub fn text_faint() -> Rgba {
        rgba(0x626a8cff)
    }
    /// Text on a bright fill (a primary button, a count badge).
    pub fn ink() -> Rgba {
        rgba(0x0a0b14ff)
    }
    /// The user: their turns, focus, selection, the primary action.
    pub fn accent() -> Hsla {
        rgba(0xff4fa3ff).into()
    }
    /// An agent doing work; running code.
    pub fn cyan() -> Hsla {
        rgba(0x2fe3ffff).into()
    }
    /// Decisions waiting on a human.
    pub fn amber() -> Hsla {
        rgba(0xffb62eff).into()
    }
    /// Done, allowed, added.
    pub fn lime() -> Hsla {
        rgba(0x7df26bff).into()
    }
    /// Failed, denied, removed.
    pub fn red() -> Hsla {
        rgba(0xff5470ff).into()
    }
    /// Changing files; the sealed VM.
    pub fn violet() -> Hsla {
        rgba(0xa98bffff).into()
    }
    /// Looking: reads and searches.
    pub fn sky() -> Hsla {
        rgba(0x5aa9ffff).into()
    }
    /// Leaving the machine: the network.
    pub fn orange() -> Hsla {
        rgba(0xff8a4cff).into()
    }
    /// A stable hue for a name, so a project keeps its colour everywhere.
    pub fn hue(name: &str) -> Hsla {
        let hues = [Self::accent(), Self::cyan(), Self::violet(), Self::lime(), Self::sky(), Self::orange()];
        let sum = name.bytes().fold(0usize, |sum, byte| sum.wrapping_mul(31).wrapping_add(usize::from(byte)));
        hues[sum % hues.len()]
    }
    /// The signature sweep, user to agent: the primary button, the pane's top edge.
    pub fn sweep(a: f32) -> Background {
        linear_gradient(
            90.,
            linear_color_stop(tint(Self::accent(), a), 0.),
            linear_color_stop(tint(Self::violet(), a), 1.),
        )
    }
    /// Code: the accents again, a little softer, plus one blue for types,
    /// so highlighted code sits in the same palette as the window.
    pub fn code(class: Class) -> Hsla {
        match class {
            Class::Comment => Self::text_faint().into(),
            Class::String => rgba(0x9af08ae6).into(),
            Class::Keyword => rgba(0xff7ab8ff).into(),
            Class::Number | Class::Constant => rgba(0xffc46eff).into(),
            Class::Function => rgba(0x6fe9ffff).into(),
            Class::Type => rgba(0xb9a3ffff).into(),
        }
    }
}

/// `color` at alpha `a`, for tints and washes of an accent.
pub fn tint(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}

/// A neon halo in `color`: what marks the one live thing in a region.
pub fn glow(color: Hsla, strength: f32) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: tint(color, 0.45 * strength),
        offset: point(px(0.), px(0.)),
        blur_radius: px(14.),
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
