//! The activity sheen: a bright white band sweeping right-to-left along a
//! sidebar row's border while its turn is working.
//!
//! The design choice: one shared phase for every working row, stepped by the
//! real frame gap like the springs, so rows in activity sweep together. The
//! band fully leaves the row before it re-enters, so one pass reads as one
//! sweep; when no turn is working the phase resets and the window draws
//! nothing again.

/// Seconds for one pass: fully off the right edge to fully off the left.
pub const PERIOD: f32 = 3.2;
/// The band's width as a fraction of the row's width.
pub const BAND: f32 = 0.45;

#[derive(Debug, Default)]
pub struct Sheen {
    phase: f32,
    working: bool,
}

impl Sheen {
    /// Advance by `dt` while any turn is working; whether the sheen moves.
    /// A window with no working turn rests, whatever the phase was.
    pub fn step(&mut self, dt: f32, working: bool) -> bool {
        self.working = working;
        if !working {
            self.phase = 0.;
            return false;
        }
        self.phase = (self.phase + dt / PERIOD) % 1.;
        true
    }

    /// Whether a turn is working: the sheen is drawn only then.
    pub fn working(&self) -> bool {
        self.working
    }

    /// The band's centre as a fraction of the row: beyond the right edge at
    /// phase 0, beyond the left edge at phase 1.
    pub fn center(&self) -> f32 {
        1. + BAND / 2. - self.phase * (1. + BAND)
    }

    /// The pass position, 0 to 1.
    pub fn phase(&self) -> f32 {
        self.phase
    }

    pub fn at_rest(&self) -> bool {
        !self.working
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_crosses_right_to_left_and_wraps() {
        let mut s = Sheen::default();
        assert!(s.at_rest());
        assert!(s.center() > 1., "the pass starts off the right edge");
        assert!(s.step(0.4, true));
        let first = s.center();
        assert!(s.step(0.4, true));
        assert!(s.center() < first, "the band moves left");
        // A full period returns to the start, off the right edge again.
        for _ in 0..12 {
            s.step(0.2, true);
        }
        assert!(s.phase() < 1e-3 || s.phase() > 1. - 1e-3, "{}", s.phase());
        assert!(s.center() > 1. - 1e-3);
    }

    #[test]
    fn the_pass_ends_off_the_left_edge() {
        let mut s = Sheen::default();
        s.step(1.6, true); // half a period: the centre of the row
        assert!((s.center() - 0.5).abs() < 1e-3, "{}", s.center());
        s.step(1.59, true);
        assert!(s.center() < 0., "the pass ends off the left edge");
    }

    #[test]
    fn no_working_turn_means_rest_and_a_fresh_pass() {
        let mut s = Sheen::default();
        s.step(1.0, true);
        assert!(!s.at_rest());
        assert!(!s.step(0.1, false), "the sheen stops with the turn");
        assert!(s.at_rest());
        assert_eq!(s.phase(), 0., "the next turn starts a fresh pass");
        assert!(!s.working());
    }
}
