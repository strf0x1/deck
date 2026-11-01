//! A damped spring for UI motion: position chases a target with velocity, so
//! a dropped card overshoots slightly and settles instead of snapping.
//!
//! The design choice: physics, not tweens. A tween restarts awkwardly when
//! its target moves mid-flight (a card displaced twice during one drag); a
//! spring just keeps chasing. Stepped in fixed substeps for stability at any
//! frame rate.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    pub pos: f32,
    pub vel: f32,
    pub target: f32,
}

const STIFFNESS: f32 = 520.;
/// Slightly under critical damping (2·√k ≈ 45.6): a small, quick overshoot.
const DAMPING: f32 = 36.;
const SUBSTEP: f32 = 1. / 240.;
/// Longest frame gap honoured; a stall doesn't fling cards.
const MAX_DT: f32 = 1. / 20.;

impl Spring {
    pub fn at(pos: f32) -> Self {
        Spring { pos, vel: 0., target: pos }
    }

    /// Advance by `dt` seconds.
    pub fn step(&mut self, dt: f32) {
        let mut left = dt.clamp(0., MAX_DT);
        while left > 0. {
            let h = left.min(SUBSTEP);
            let accel = STIFFNESS * (self.target - self.pos) - DAMPING * self.vel;
            self.vel += accel * h;
            self.pos += self.vel * h;
            left -= h;
        }
        if self.at_rest() {
            self.pos = self.target;
            self.vel = 0.;
        }
    }

    pub fn at_rest(&self) -> bool {
        (self.target - self.pos).abs() < 0.25 && self.vel.abs() < 4.
    }

    /// Jump with no motion (the dragged card follows the pointer exactly).
    pub fn set(&mut self, pos: f32) {
        *self = Spring::at(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles_on_the_target_with_a_small_overshoot() {
        let mut s = Spring::at(0.);
        s.target = 100.;
        let mut peak: f32 = 0.;
        for _ in 0..120 {
            s.step(1. / 60.);
            peak = peak.max(s.pos);
        }
        assert!(s.at_rest());
        assert_eq!(s.pos, 100.);
        assert!(peak > 100. && peak < 106., "overshoot {peak}");
    }

    #[test]
    fn a_long_stall_is_clamped() {
        let mut s = Spring::at(0.);
        s.target = 100.;
        s.step(5.);
        assert!(s.pos < 100., "one stalled frame must not teleport: {}", s.pos);
    }
}
