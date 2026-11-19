//! The state mark's crossfade: when a row's mark changes shape, the old
//! glyph fades out as the new one fades in, on the window's one spring
//! tuning.
//!
//! The design choice: one fade per lane, started by observing a change and
//! stepped by the real frame gap like the other motion. Only opacity moves —
//! the mark's box is fixed, so nothing re-measures — and a settled fade is
//! dropped, so an idle window rests.

use super::board::LaneState;
use super::spring::Spring;
use std::collections::HashMap;

/// A fade's span: 0 the old mark, 100 the new one.
const FULL: f32 = 100.;

#[derive(Debug, Default)]
pub struct Marks {
    /// The last state observed per lane.
    seen: HashMap<usize, LaneState>,
    fades: HashMap<usize, Fade>,
}

#[derive(Debug)]
struct Fade {
    from: LaneState,
    spring: Spring,
}

impl Marks {
    /// Record the mark a lane wears; a change starts a crossfade from the
    /// previous mark. A lane's first observation draws no fade.
    pub fn observe(&mut self, id: usize, state: LaneState) {
        if let Some(old) = self.seen.insert(id, state)
            && old != state
        {
            self.fades.insert(id, Fade { from: old, spring: Spring { target: FULL, ..Spring::at(0.) } });
        }
    }

    /// Advance by `dt`; whether any crossfade still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        for f in self.fades.values_mut() {
            f.spring.step(dt);
        }
        self.fades.retain(|_, f| !f.spring.at_rest());
        !self.fades.is_empty()
    }

    /// The crossfade to draw on lane `id`: the old mark, and how far (0 to 1)
    /// the new one has taken over.
    pub fn fade(&self, id: usize) -> Option<(LaneState, f32)> {
        self.fades.get(&id).map(|f| (f.from, (f.spring.pos / FULL).clamp(0., 1.)))
    }

    /// Forget the lanes the board no longer shows.
    pub fn retain(&mut self, ids: &[usize]) {
        self.seen.retain(|id, _| ids.contains(id));
        self.fades.retain(|id, _| ids.contains(id));
    }

    pub fn at_rest(&self) -> bool {
        self.fades.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_mark_does_not_fade() {
        let mut m = Marks::default();
        m.observe(1, LaneState::Idle);
        m.observe(1, LaneState::Idle);
        assert!(m.fade(1).is_none());
        assert!(m.at_rest());
    }

    #[test]
    fn a_change_crossfades_from_the_old_mark_and_rests() {
        let mut m = Marks::default();
        m.observe(1, LaneState::Idle);
        m.observe(1, LaneState::Working);
        let (from, t) = m.fade(1).expect("the mark changed");
        assert_eq!(from, LaneState::Idle);
        assert_eq!(t, 0., "the new mark starts invisible");
        assert!(!m.at_rest());
        assert!(m.step(1. / 60.));
        let (_, t) = m.fade(1).unwrap();
        assert!(t > 0. && t <= 1., "the new mark arrives: {t}");
        for _ in 0..120 {
            m.step(1. / 60.);
        }
        assert!(m.fade(1).is_none(), "a settled fade is dropped");
        assert!(m.at_rest());
        assert!(!m.step(1. / 60.));
    }

    #[test]
    fn a_second_change_mid_fade_starts_from_the_mark_it_was_becoming() {
        let mut m = Marks::default();
        m.observe(1, LaneState::Idle);
        m.observe(1, LaneState::Working);
        m.step(1. / 60.);
        m.observe(1, LaneState::Waiting);
        let (from, t) = m.fade(1).expect("the mark changed again");
        assert_eq!(from, LaneState::Working);
        assert_eq!(t, 0., "the crossfade restarts, never snaps");
    }

    #[test]
    fn lanes_the_board_drops_are_forgotten() {
        let mut m = Marks::default();
        m.observe(1, LaneState::Idle);
        m.observe(1, LaneState::Working);
        m.observe(2, LaneState::Idle);
        m.retain(&[2]);
        assert!(m.fade(1).is_none());
        assert!(m.at_rest());
        m.observe(2, LaneState::Working);
        assert!(m.fade(2).is_some(), "a kept lane still crossfades");
    }
}
