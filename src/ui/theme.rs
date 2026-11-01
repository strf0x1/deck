//! Deck's colours and type, as named tokens. Every view takes colours from
//! here, never from a literal, so the look can be retuned in one file.
//!
//! The design choice: glass is translucency over a compositor blur, so the
//! surfaces are dark with alpha, and neon is reserved for edges and state
//! (an accent tick, a live dot), never for fills or text glow.

use gpui::{Hsla, Rgba, rgba};

pub const FONT: &str = "JetBrainsMono Nerd Font";

pub struct Tokens;

impl Tokens {
    /// The whole window: translucent so the compositor's blur shows through.
    pub fn base() -> Rgba {
        rgba(0x070a12a8)
    }
    pub fn panel() -> Rgba {
        rgba(0x111827b3)
    }
    pub fn panel_raised() -> Rgba {
        rgba(0x1a2236d9)
    }
    pub fn edge() -> Rgba {
        rgba(0x8ea2d22e)
    }
    pub fn edge_strong() -> Rgba {
        rgba(0x8ea2d266)
    }
    pub fn text() -> Rgba {
        rgba(0xd8e0f0ff)
    }
    pub fn text_dim() -> Rgba {
        rgba(0x7d89a6ff)
    }
    pub fn text_faint() -> Rgba {
        rgba(0x4b5672ff)
    }
    /// Primary accent: live state, focus, the agent.
    pub fn cyan() -> Hsla {
        rgba(0x2de2e6ff).into()
    }
    /// Secondary accent: the user, drag lift.
    pub fn magenta() -> Hsla {
        rgba(0xff3caaff).into()
    }
    /// Decisions waiting on a human.
    pub fn amber() -> Hsla {
        rgba(0xf6c177ff).into()
    }
    pub fn lime() -> Hsla {
        rgba(0x9ef01aff).into()
    }
    pub fn red() -> Hsla {
        rgba(0xff5d73ff).into()
    }
}
