//! The ask card's arrival: the one moment that blocks on a human gets the
//! loudest entrance in the window — a spring-in and one amber pass.
//!
//! The design choice: opacity on the shared spring (GPUI has no transform
//! to scale a div, and the card's height must never animate), plus one
//! pass of the waiting hue around the card's outline (`band.rs`). Started
//! when a question appears — including the first observation, since a card
//! the window has never drawn is arriving. Answered or gone, it resets;
//! the next question arrives again. Stepped by the real frame gap; rests.

use super::sheen::BAND;
use super::spring::Spring;

/// A spring's span: 0 hidden, 100 shown.
const FULL: f32 = 100.;
/// Seconds for the amber pass: fully off the right edge to fully off the left.
const FLASH: f32 = 0.8;

#[derive(Debug)]
pub struct Arrival {
    spring: Spring,
    flash: Option<f32>,
    present: bool,
}

impl Default for Arrival {
    fn default() -> Self {
        Arrival { spring: Spring::at(FULL), flash: None, present: false }
    }
}

impl Arrival {
    /// Observe whether the selected session has an open question. An
    /// appearance starts the entrance; a disappearance resets it.
    pub fn observe(&mut self, present: bool) {
        if present && !self.present {
            self.spring = Spring { target: FULL, ..Spring::at(0.) };
            self.flash = Some(0.);
        } else if !present {
            self.spring = Spring::at(FULL);
            self.flash = None;
        }
        self.present = present;
    }

    /// The card's opacity, 0 to 1.
    pub fn opacity(&self) -> f32 {
        (self.spring.pos / FULL).clamp(0., 1.)
    }

    /// The amber pass's centre as a fraction of the card, while it runs.
    pub fn flash(&self) -> Option<f32> {
        let e = self.flash?;
        Some(1. + BAND / 2. - (e / FLASH) * (1. + BAND))
    }

    /// Advance by `dt`; whether the entrance still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        self.spring.step(dt);
        if let Some(e) = self.flash.as_mut() {
            *e += dt;
            if *e >= FLASH {
                self.flash = None;
            }
        }
        !self.at_rest()
    }

    pub fn at_rest(&self) -> bool {
        self.spring.at_rest() && self.flash.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_arrival_springs_in_and_flashes_once() {
        let mut a = Arrival::default();
        assert!(a.at_rest());
        assert_eq!(a.opacity(), 1., "no card, nothing to fade");
        a.observe(true);
        assert_eq!(a.opacity(), 0., "the card starts invisible");
        let first = a.flash().expect("the amber pass starts");
        assert!(first > 1., "off the right edge");
        assert!(!a.at_rest());
        a.step(1. / 60.);
        assert!(a.opacity() > 0., "the card arrives");
        assert!(a.flash().unwrap() < first, "the pass moves left");
        for _ in 0..120 {
            a.step(1. / 60.);
        }
        assert_eq!(a.opacity(), 1.);
        assert!(a.flash().is_none(), "one pass only");
        assert!(a.at_rest());
    }

    #[test]
    fn a_question_already_open_on_first_observation_is_arriving() {
        let mut a = Arrival::default();
        a.observe(true);
        assert_eq!(a.opacity(), 0., "a card the window has never drawn arrives");
        assert!(a.flash().is_some());
    }

    #[test]
    fn an_answer_resets_and_the_next_question_arrives_again() {
        let mut a = Arrival::default();
        a.observe(true);
        a.step(1. / 60.);
        a.observe(false);
        assert_eq!(a.opacity(), 1.);
        assert!(a.flash().is_none());
        assert!(a.at_rest());
        a.observe(true);
        assert_eq!(a.opacity(), 0., "the next question gets its own entrance");
        assert!(a.flash().is_some());
    }

    #[test]
    fn no_question_stays_quiet() {
        let mut a = Arrival::default();
        a.observe(false);
        a.observe(false);
        assert!(a.at_rest());
        assert!(!a.step(0.1));
        assert_eq!(a.opacity(), 1.);
    }
}
