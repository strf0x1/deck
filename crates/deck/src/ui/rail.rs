//! The project rail: a column of projects that stays narrow until the pointer
//! reaches it, then opens to show names, and closes again when it leaves.
//! A pin keeps it open (ADR 0010 §1, §9).
//!
//! The design choice: the rail's width and its active-project marker are
//! [`Spring`]s with the same tuning as the lane cards, so opening the rail and
//! reordering sessions share one motion. The rail floats over the content
//! when open instead of pushing it, so the transcript never reflows mid-flight.

use super::spring::Spring;

pub const COLLAPSED: f32 = 52.;
pub const EXPANDED: f32 = 200.;
pub const ITEM_H: f32 = 40.;

pub struct Rail {
    pub width: Spring,
    /// The active marker's offset from the first project.
    pub marker: Spring,
    /// The selected project's index, or none while no project is open.
    active: Option<usize>,
    hovered: bool,
    pinned: bool,
}

impl Rail {
    pub fn new() -> Self {
        Rail { width: Spring::at(COLLAPSED), marker: Spring::at(0.), active: None, hovered: false, pinned: false }
    }

    pub fn active(&self) -> Option<usize> {
        self.active
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the rail is open or opening.
    pub fn expanded(&self) -> bool {
        self.hovered || self.pinned
    }

    /// 0 closed, 1 open; overshoots slightly as the spring settles.
    pub fn openness(&self) -> f32 {
        (self.width.pos - COLLAPSED) / (EXPANDED - COLLAPSED)
    }

    pub fn hover(&mut self, on: bool) {
        self.hovered = on;
        self.retarget();
    }

    pub fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
        self.retarget();
    }

    /// Mark project `index` (or none) as the selected one.
    pub fn select(&mut self, index: Option<usize>) {
        self.active = index;
        self.marker.target = index.map_or(0., |i| i as f32 * ITEM_H);
    }

    fn retarget(&mut self) {
        self.width.target = if self.expanded() { EXPANDED } else { COLLAPSED };
    }

    /// Advance motion by `dt` seconds; true while anything still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        self.width.step(dt);
        self.marker.step(dt);
        !self.width.at_rest() || !self.marker.at_rest()
    }
}

impl Default for Rail {
    fn default() -> Self {
        Rail::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(r: &mut Rail) {
        for _ in 0..600 {
            if !r.step(1. / 60.) {
                return;
            }
        }
        panic!("rail never settled");
    }

    #[test]
    fn hovering_opens_and_leaving_closes() {
        let mut r = Rail::new();
        r.hover(true);
        assert!(r.step(1. / 60.), "opening is animated, not a jump");
        assert!(r.width.pos > COLLAPSED && r.width.pos < EXPANDED);
        settle(&mut r);
        assert_eq!(r.width.pos, EXPANDED);
        assert_eq!(r.openness(), 1.);
        r.hover(false);
        settle(&mut r);
        assert_eq!(r.width.pos, COLLAPSED);
    }

    #[test]
    fn a_pin_keeps_it_open_after_the_pointer_leaves() {
        let mut r = Rail::new();
        r.hover(true);
        r.toggle_pin();
        r.hover(false);
        settle(&mut r);
        assert_eq!(r.width.pos, EXPANDED);
        r.toggle_pin();
        settle(&mut r);
        assert_eq!(r.width.pos, COLLAPSED);
    }

    #[test]
    fn selecting_slides_the_marker_and_none_returns_it_to_the_top() {
        let mut r = Rail::new();
        assert_eq!(r.active(), None);
        r.select(Some(0));
        settle(&mut r);
        assert_eq!((r.active(), r.marker.pos), (Some(0), 0.));
        r.select(Some(2));
        settle(&mut r);
        assert_eq!((r.active(), r.marker.pos), (Some(2), 2. * ITEM_H));
        r.select(None);
        settle(&mut r);
        assert_eq!((r.active(), r.marker.pos), (None, 0.));
    }
}
