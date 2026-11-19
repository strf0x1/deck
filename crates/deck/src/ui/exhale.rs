//! The turn's exhale: when a working turn ends, its row's white sheen fades
//! where it stood while one green pulse crosses the border once.
//!
//! The design choice: one entry per lane, started when a turn the window saw
//! working ends — even if its last observed phase was a permission pause, so
//! an answer and the turn's end landing in one frame still exhale. Stepped
//! by the real frame gap like the springs. The white band keeps moving from
//! the centre the shared sheen left it at, so nothing jumps when the sheen
//! resets; the green pulse — the done hue — makes exactly one pass, so the
//! exhale always rests. A new turn cancels it, and the view draws it only on
//! a row that is not asking.

use super::sheen::{BAND, PERIOD};
use std::collections::{HashMap, HashSet};

/// Seconds the white sheen takes to fade once the turn ends.
pub const FADE: f32 = 0.4;
/// Seconds for the done pulse: one full pass, right to left.
pub const PULSE: f32 = 0.9;

#[derive(Debug, Default)]
pub struct Exhale {
    /// The lanes seen working at the last observe.
    working: HashSet<usize>,
    /// The lanes whose turn was seen working and has not ended yet. A
    /// permission pause keeps the claim; only the end spends it.
    seen: HashSet<usize>,
    settling: HashMap<usize, Entry>,
}

#[derive(Debug)]
struct Entry {
    elapsed: f32,
    /// The shared sheen's centre when the turn ended.
    center: f32,
}

/// One row's exhale for this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// The fading white band's centre, as a fraction of the row.
    pub white_center: f32,
    /// The white band's opacity: 1 at the turn's end, 0 after [`FADE`].
    pub white_alpha: f32,
    /// The green pulse's centre: off the right edge at the turn's end,
    /// off the left edge once [`PULSE`] has passed.
    pub pulse_center: f32,
}

impl Exhale {
    /// Record a lane's turn phase; `center` is the shared sheen's centre,
    /// read before a resting sheen resets it. `working` means a turn is in
    /// progress, `ended` means it finished (a permission pause is neither).
    /// Ending a turn the window saw working starts an exhale; starting a
    /// new turn cancels one in flight.
    pub fn observe(&mut self, id: usize, working: bool, ended: bool, center: f32) {
        if working {
            self.working.insert(id);
            self.seen.insert(id);
            self.settling.remove(&id);
            return;
        }
        self.working.remove(&id);
        if ended && self.seen.remove(&id) {
            self.settling.insert(id, Entry { elapsed: 0., center });
        }
    }

    /// Advance by `dt`; whether any exhale still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        for e in self.settling.values_mut() {
            e.elapsed += dt;
            e.center -= dt / PERIOD * (1. + BAND);
        }
        self.settling.retain(|_, e| e.elapsed < PULSE);
        !self.settling.is_empty()
    }

    /// The exhale to draw on lane `id`, if its turn just ended.
    pub fn frame(&self, id: usize) -> Option<Frame> {
        let e = self.settling.get(&id)?;
        Some(Frame {
            white_center: e.center,
            white_alpha: (1. - e.elapsed / FADE).clamp(0., 1.),
            pulse_center: 1. + BAND / 2. - (e.elapsed / PULSE) * (1. + BAND),
        })
    }

    /// Forget the lanes the board no longer shows.
    pub fn retain(&mut self, ids: &[usize]) {
        self.working.retain(|id| ids.contains(id));
        self.seen.retain(|id| ids.contains(id));
        self.settling.retain(|id, _| ids.contains(id));
    }

    pub fn at_rest(&self) -> bool {
        self.settling.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(e: &mut Exhale, id: usize) {
        e.observe(id, true, false, 0.5);
    }
    fn pause(e: &mut Exhale, id: usize) {
        e.observe(id, false, false, 0.5);
    }
    fn end(e: &mut Exhale, id: usize) {
        e.observe(id, false, true, 0.5);
    }

    #[test]
    fn a_turn_ending_starts_the_exhale_where_the_sheen_stood() {
        let mut e = Exhale::default();
        assert!(e.at_rest());
        work(&mut e, 1);
        assert!(e.frame(1).is_none(), "a working turn has no exhale yet");
        end(&mut e, 1);
        let f = e.frame(1).expect("the turn ended");
        assert_eq!(f.white_center, 0.5, "the white band keeps its place");
        assert_eq!(f.white_alpha, 1.);
        assert!(f.pulse_center > 1., "the pulse starts off the right edge");
        assert!(!e.at_rest());
    }

    #[test]
    fn a_permission_pause_keeps_the_claim_until_the_turn_ends() {
        let mut e = Exhale::default();
        work(&mut e, 1);
        pause(&mut e, 1);
        pause(&mut e, 1);
        assert!(e.frame(1).is_none(), "a pause is not an end");
        end(&mut e, 1);
        assert!(e.frame(1).is_some(), "the end after a pause still exhales");
    }

    #[test]
    fn an_end_the_window_never_saw_working_does_not_exhale() {
        let mut e = Exhale::default();
        pause(&mut e, 1);
        end(&mut e, 1);
        assert!(e.frame(1).is_none(), "no known activity, no claim");
        assert!(e.at_rest());
    }

    #[test]
    fn the_white_band_fades_before_the_pulse_ends() {
        let mut e = Exhale::default();
        work(&mut e, 1);
        end(&mut e, 1);
        assert!(e.step(FADE));
        let f = e.frame(1).expect("the pulse is still crossing");
        assert_eq!(f.white_alpha, 0., "the white band has faded");
        assert!(f.pulse_center < 1. + BAND / 2., "the pulse moves left");
        // The white band keeps sweeping at the sheen's own speed as it fades.
        let expected = 0.5 - FADE / PERIOD * (1. + BAND);
        assert!((f.white_center - expected).abs() < 1e-4, "{}", f.white_center);
    }

    #[test]
    fn the_pulse_crosses_once_and_rests() {
        let mut e = Exhale::default();
        work(&mut e, 1);
        end(&mut e, 1);
        let mut last = e.frame(1).unwrap().pulse_center;
        let mut steps = 0;
        while e.step(PULSE / 20.) {
            let f = e.frame(1).expect("an entry lives until its pass ends");
            assert!(f.pulse_center < last, "the pulse only moves left");
            last = f.pulse_center;
            steps += 1;
            assert!(steps < 40, "one pass, not a loop");
        }
        assert!(last < 0., "the pass ends off the left edge");
        assert!(e.frame(1).is_none());
        assert!(e.at_rest());
        assert!(!e.step(0.1), "a rested exhale draws nothing");
    }

    #[test]
    fn a_new_turn_cancels_the_exhale() {
        let mut e = Exhale::default();
        work(&mut e, 1);
        end(&mut e, 1);
        assert!(e.frame(1).is_some());
        work(&mut e, 1);
        assert!(e.frame(1).is_none(), "the live sheen takes over");
        assert!(e.at_rest());
    }

    #[test]
    fn only_a_transition_starts_one() {
        let mut e = Exhale::default();
        pause(&mut e, 1);
        pause(&mut e, 1);
        work(&mut e, 2);
        work(&mut e, 2);
        assert!(e.at_rest(), "no turn ended, no exhale");
    }

    #[test]
    fn lanes_the_board_drops_are_forgotten() {
        let mut e = Exhale::default();
        work(&mut e, 1);
        end(&mut e, 1);
        work(&mut e, 2);
        e.retain(&[2]);
        assert!(e.frame(1).is_none());
        end(&mut e, 2);
        assert!(e.frame(2).is_some(), "a kept lane still exhales");
    }
}
