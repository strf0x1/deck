//! Transcript motion: a card's body fading in as it opens, the jump pill
//! rising into view, and a tool's finish blooming once around its card.
//!
//! The design choice: the same springs as the board and rail (`spring.rs`),
//! run from 0 to 100 so their pixel-sized rest threshold reads as a
//! percent. Only opacity, the pill's offset and the bloom's band move. A
//! body's height is never animated, because the virtualized list measures
//! rows once and an animated height would mean measuring every frame. The
//! bloom is one pass of the outcome hue (`band.rs` paints it), started by a
//! finish the window saw running; a first observation never blooms, so
//! replayed history stays quiet.

use super::sheen::BAND;
use super::spring::Spring;
use deck_acp::ToolStatus;
use std::collections::{HashMap, HashSet};

/// A spring's span: 0 hidden, 100 shown.
const FULL: f32 = 100.;
/// Seconds for one bloom pass: fully off the right edge to fully off the left.
const BLOOM: f32 = 0.8;

#[derive(Debug)]
pub struct Motion {
    /// Bodies still fading in, by row key; a key not here is fully shown.
    reveals: HashMap<String, Spring>,
    pill: Spring,
    /// The last status observed per tool call.
    tools_seen: HashMap<String, ToolStatus>,
    /// Finishes still blooming, by tool call id.
    blooms: HashMap<String, Bloom>,
}

#[derive(Debug)]
struct Bloom {
    elapsed: f32,
    failed: bool,
}

impl Default for Motion {
    fn default() -> Self {
        Motion { reveals: HashMap::new(), pill: Spring::at(0.), tools_seen: HashMap::new(), blooms: HashMap::new() }
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

    /// Observe the session's tools. A finish (done or failed) after a seen
    /// running start blooms once in its outcome hue; a first observation
    /// never blooms, so replayed history stays quiet. Tools no longer
    /// listed are forgotten.
    pub fn tools<'a>(&mut self, tools: impl Iterator<Item = (&'a str, ToolStatus)>) {
        let mut live: HashSet<&str> = HashSet::new();
        for (id, status) in tools {
            live.insert(id);
            match self.tools_seen.get_mut(id) {
                Some(prev) if *prev != status => {
                    let was_running = *prev == ToolStatus::Running;
                    *prev = status;
                    if was_running && matches!(status, ToolStatus::Done | ToolStatus::Failed) {
                        self.blooms.insert(id.to_string(), Bloom { elapsed: 0., failed: status == ToolStatus::Failed });
                    }
                }
                Some(_) => {}
                None => {
                    self.tools_seen.insert(id.to_string(), status);
                }
            }
        }
        self.tools_seen.retain(|id, _| live.contains(id.as_str()));
        self.blooms.retain(|id, _| live.contains(id.as_str()));
    }

    /// The bloom on a tool's card: the band's centre as a fraction of the
    /// card, and whether the outcome was failure. `None` when nothing
    /// blooms.
    pub fn bloom(&self, id: &str) -> Option<(f32, bool)> {
        let b = self.blooms.get(id)?;
        Some((1. + BAND / 2. - (b.elapsed / BLOOM) * (1. + BAND), b.failed))
    }

    /// Advance by `dt`; whether anything still moves. Settled bodies and
    /// finished blooms are dropped.
    pub fn step(&mut self, dt: f32) -> bool {
        self.pill.step(dt);
        for s in self.reveals.values_mut() {
            s.step(dt);
        }
        self.reveals.retain(|_, s| !s.at_rest());
        for b in self.blooms.values_mut() {
            b.elapsed += dt;
        }
        self.blooms.retain(|_, b| b.elapsed < BLOOM);
        !self.at_rest()
    }

    pub fn at_rest(&self) -> bool {
        self.pill.at_rest() && self.reveals.is_empty() && self.blooms.is_empty()
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

    #[test]
    fn a_finish_blooms_once_right_to_left_and_rests() {
        let mut m = Motion::default();
        m.tools([("t", ToolStatus::Running)].into_iter());
        assert!(m.bloom("t").is_none(), "a running tool has no bloom yet");
        m.tools([("t", ToolStatus::Done)].into_iter());
        let (first, failed) = m.bloom("t").expect("the finish blooms");
        assert!(!failed);
        assert!(first > 1., "the pass starts off the right edge");
        assert!(!m.at_rest());
        m.step(BLOOM / 2.);
        let (mid, _) = m.bloom("t").unwrap();
        assert!(mid < first, "the band moves left");
        m.step(BLOOM);
        assert!(m.bloom("t").is_none(), "one pass only");
        assert!(m.at_rest());
    }

    #[test]
    fn a_failed_finish_blooms_in_the_failure_hue() {
        let mut m = Motion::default();
        m.tools([("t", ToolStatus::Running)].into_iter());
        m.tools([("t", ToolStatus::Failed)].into_iter());
        let (_, failed) = m.bloom("t").expect("a failure blooms too");
        assert!(failed);
    }

    #[test]
    fn replayed_history_stays_quiet() {
        let mut m = Motion::default();
        m.tools([("done", ToolStatus::Done), ("failed", ToolStatus::Failed)].into_iter());
        assert!(m.bloom("done").is_none(), "a first observation is not new work");
        assert!(m.bloom("failed").is_none());
        assert!(m.at_rest());
    }

    #[test]
    fn only_a_finish_starts_one() {
        let mut m = Motion::default();
        m.tools([("t", ToolStatus::Running)].into_iter());
        m.tools([("t", ToolStatus::Running)].into_iter());
        assert!(m.bloom("t").is_none());
        m.tools([("t", ToolStatus::Done)].into_iter());
        m.step(BLOOM / 2.);
        m.tools([("t", ToolStatus::Done)].into_iter());
        let (center, _) = m.bloom("t").unwrap();
        assert!(center < 1., "a repeated status does not restart the pass");
    }

    #[test]
    fn tools_the_session_drops_are_forgotten() {
        let mut m = Motion::default();
        m.tools([("t", ToolStatus::Running)].into_iter());
        m.tools([("t", ToolStatus::Done)].into_iter());
        assert!(m.bloom("t").is_some());
        m.tools([].into_iter());
        assert!(m.bloom("t").is_none());
        assert!(m.at_rest());
        m.tools([("t", ToolStatus::Done)].into_iter());
        assert!(m.bloom("t").is_none(), "a returning tool is a first observation again");
    }
}
