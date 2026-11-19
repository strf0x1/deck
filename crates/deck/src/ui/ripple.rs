//! The send ripple: one purple gesture from the composer to the new user
//! turn — the user's hue spent once as motion, cause then effect.
//!
//! The design choice: one pass, two surfaces. The composer's outline
//! sweeps first; the new bubble's outline follows a beat later, so the
//! eye reads the prompt leaving the input and arriving as the card. Both
//! are single right-to-left passes of the shared band (`band.rs`), stepped
//! by the real frame gap; the gesture rests when they end. The ripple
//! names the task it was sent from, so a switch mid-pass draws nothing on
//! the wrong session, and a send mid-pass starts the gesture over.

use super::sheen::BAND;

/// Seconds for the whole gesture.
const SPAN: f32 = 0.9;
/// The composer's pass, in seconds from the send: it leads.
const COMPOSER: (f32, f32) = (0., 0.6);
/// The bubble's pass, a beat behind: the effect follows the cause.
const BUBBLE: (f32, f32) = (0.25, 0.85);

#[derive(Debug, Default)]
pub struct Ripple {
    elapsed: f32,
    active: bool,
    /// The task the prompt was sent from; `None` for the idle session.
    task: Option<String>,
}

impl Ripple {
    /// A prompt was sent: start the gesture from the composer.
    pub fn send(&mut self, task: Option<String>) {
        self.elapsed = 0.;
        self.active = true;
        self.task = task;
    }

    /// Advance by `dt`; whether the gesture still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        if !self.active {
            return false;
        }
        self.elapsed += dt;
        if self.elapsed >= SPAN {
            self.active = false;
        }
        self.active
    }

    /// Whether the gesture belongs to the session the window shows.
    pub fn for_task(&self, task: Option<&str>) -> bool {
        self.active && self.task.as_deref() == task
    }

    /// The composer band's centre while its pass runs.
    pub fn composer(&self) -> Option<f32> {
        self.active.then(|| pass(self.elapsed, COMPOSER)).flatten()
    }

    /// The new bubble's band centre while its pass runs.
    pub fn bubble(&self) -> Option<f32> {
        self.active.then(|| pass(self.elapsed, BUBBLE)).flatten()
    }

    pub fn at_rest(&self) -> bool {
        !self.active
    }
}

/// One pass: off the right edge at the start, off the left edge at the end.
fn pass(elapsed: f32, (start, end): (f32, f32)) -> Option<f32> {
    if elapsed < start || elapsed >= end {
        return None;
    }
    let p = (elapsed - start) / (end - start);
    Some(1. + BAND / 2. - p * (1. + BAND))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_leads_with_the_composer_and_the_bubble_follows() {
        let mut r = Ripple::default();
        assert!(r.at_rest());
        assert!(r.composer().is_none());
        r.send(Some("task".into()));
        assert!(!r.at_rest());
        let first = r.composer().expect("the composer leads");
        assert!(first > 1., "the pass starts off the right edge");
        assert!(r.bubble().is_none(), "the bubble waits a beat");
        r.step(0.3);
        assert!(r.composer().unwrap() < first, "the composer band moves left");
        let bubble = r.bubble().expect("the bubble follows");
        assert!(bubble > 1., "its pass starts at the right edge too");
    }

    #[test]
    fn both_passes_end_and_the_gesture_rests() {
        let mut r = Ripple::default();
        r.send(None);
        let mut steps = 0;
        while r.step(SPAN / 20.) {
            steps += 1;
            assert!(steps < 40, "one gesture, not a loop");
        }
        assert!(r.composer().is_none());
        assert!(r.bubble().is_none());
        assert!(r.at_rest());
        assert!(!r.step(0.1), "a rested ripple draws nothing");
    }

    #[test]
    fn a_send_mid_pass_starts_the_gesture_over() {
        let mut r = Ripple::default();
        r.send(None);
        r.step(0.5);
        assert!(r.bubble().is_some());
        r.send(None);
        assert!(r.bubble().is_none(), "the new send leads from the composer again");
        assert!(r.composer().unwrap() > 1.);
    }

    #[test]
    fn the_gesture_belongs_to_the_session_it_was_sent_from() {
        let mut r = Ripple::default();
        r.send(Some("a".into()));
        assert!(r.for_task(Some("a")));
        assert!(!r.for_task(Some("b")), "a switch mid-pass draws nothing");
        assert!(!r.for_task(None));
        let mut idle = Ripple::default();
        idle.send(None);
        assert!(idle.for_task(None), "the idle session's own send");
        assert!(!idle.for_task(Some("a")));
    }
}
