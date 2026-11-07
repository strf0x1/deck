//! Transcript motion: a card's body fading in as it opens, and the jump
//! pill rising into view.
//!
//! The design choice: the same springs as the board and rail (`spring.rs`),
//! run from 0 to 100 so their pixel-sized rest threshold reads as a
//! percent. Only opacity and the pill's offset move. A body's height is
//! never animated, because the virtualized list measures rows once and an
//! animated height would mean measuring every frame.

use super::spring::Spring;
use std::collections::HashMap;

/// A spring's span: 0 hidden, 100 shown.
const FULL: f32 = 100.;

#[derive(Debug)]
pub struct Motion {
    /// Bodies still fading in, by row key; a key not here is fully shown.
    reveals: HashMap<String, Spring>,
    pill: Spring,
}

impl Default for Motion {
    fn default() -> Self {
        Motion { reveals: HashMap::new(), pill: Spring::at(0.) }
    }
}

impl Motion {
    /// Start fading in the body of row `key`.
    pub fn open(&mut self, key: &str) {
        self.reveals.insert(key.to_string(), Spring { target: FULL, ..Spring::at(0.) });
    }

    /// How shown row `key`'s body is, 0 to 1.
    pub fn reveal(&self, key: &str) -> f32 {
        self.reveals.get(key).map_or(1., |s| (s.pos / FULL).clamp(0., 1.))
    }

    /// Aim the pill at shown or hidden.
    pub fn pill_to(&mut self, shown: bool) {
        self.pill.target = if shown { FULL } else { 0. };
    }

    /// How shown the pill is, 0 to 1.
    pub fn pill(&self) -> f32 {
        (self.pill.pos / FULL).clamp(0., 1.)
    }

    /// Advance by `dt`; whether anything still moves. Settled bodies are dropped.
    pub fn step(&mut self, dt: f32) -> bool {
        self.pill.step(dt);
        for s in self.reveals.values_mut() {
            s.step(dt);
        }
        self.reveals.retain(|_, s| !s.at_rest());
        !self.at_rest()
    }

    pub fn at_rest(&self) -> bool {
        self.pill.at_rest() && self.reveals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opened_body_fades_in_and_is_then_forgotten() {
        let mut m = Motion::default();
        assert_eq!(m.reveal("a"), 1.);
        m.open("a");
        assert_eq!(m.reveal("a"), 0.);
        m.step(1. / 60.);
        let partway = m.reveal("a");
        assert!(partway > 0. && partway < 1., "{partway}");
        for _ in 0..120 {
            m.step(1. / 60.);
        }
        assert!(m.at_rest());
        assert_eq!(m.reveal("a"), 1.);
    }

    #[test]
    fn the_pill_rises_and_falls() {
        let mut m = Motion::default();
        m.pill_to(true);
        assert!(m.step(1. / 60.));
        for _ in 0..120 {
            m.step(1. / 60.);
        }
        assert_eq!(m.pill(), 1.);
        m.pill_to(false);
        for _ in 0..120 {
            m.step(1. / 60.);
        }
        assert_eq!(m.pill(), 0.);
    }
}
