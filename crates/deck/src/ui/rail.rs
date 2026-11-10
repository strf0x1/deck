//! The sidebar's open/close state: it stays narrow until the pointer reaches
//! it, then opens to full width and closes again when it leaves. A pin keeps
//! it open (ADR 0010 §1, §7, §9 as amended 2026-11-09).
//!
//! The design choice: the sidebar's width and its active-project marker are
//! [`Spring`]s with the same tuning as the lane cards, so opening the sidebar
//! and reordering sessions share one motion. Hovered, it floats over the
//! transcript; pinned, it stays open and takes its width from the transcript
//! instead of covering it, which is what [`Rail::reserved`] tells the layout.

use super::spring::Spring;

pub const COLLAPSED: f32 = 52.;
/// The open width: about the old sessions column's, because it now holds the
/// task list.
pub const EXPANDED: f32 = 272.;
pub const ITEM_H: f32 = 40.;

pub struct Rail {
    pub width: Spring,
    /// The active marker's offset from the first project.
    pub marker: Spring,
    /// The selected project's index, or none while no project is open.
    active: Option<usize>,
    hovered: bool,
    pinned: bool,
    /// The selected project's task list is folded closed.
    collapsed: bool,
}

impl Rail {
    pub fn new() -> Self {
        Rail {
            width: Spring::at(COLLAPSED),
            marker: Spring::at(0.),
            active: None,
            hovered: false,
            pinned: false,
            collapsed: false,
        }
    }

    /// Whether the selected project's list is folded.
    pub fn collapsed(&self) -> bool {
        self.collapsed
    }

    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
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

    /// The width the layout gives the sidebar: all of it while pinned, so the
    /// transcript reflows around it, and only the narrow strip otherwise, so
    /// the hover-open sidebar floats over the transcript.
    pub fn reserved(&self) -> f32 {
        if self.pinned { self.width.pos } else { COLLAPSED }
    }

    pub fn hover(&mut self, on: bool) {
        self.hovered = on;
        self.retarget();
    }

    pub fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
        self.retarget();
    }

    /// Mark project `index` (or none) as the selected one. A different
    /// project opens its list again; re-selecting the same one keeps the fold.
    pub fn select(&mut self, index: Option<usize>) {
        if self.active != index {
            self.collapsed = false;
        }
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
    fn a_hovered_sidebar_floats_so_the_layout_keeps_only_the_narrow_strip() {
        let mut r = Rail::new();
        r.hover(true);
        assert!(r.step(1. / 60.), "opening is animated, not a jump");
        assert!(r.width.pos > COLLAPSED, "the sidebar itself opens");
        assert_eq!(r.reserved(), COLLAPSED, "but the transcript keeps its width");
        settle(&mut r);
        assert_eq!((r.width.pos, r.reserved()), (EXPANDED, COLLAPSED));
    }

    #[test]
    fn a_pinned_sidebar_takes_its_width_from_the_transcript() {
        let mut r = Rail::new();
        r.hover(true);
        r.toggle_pin();
        settle(&mut r);
        assert_eq!(r.reserved(), EXPANDED);
        r.hover(false);
        settle(&mut r);
        assert_eq!(r.reserved(), EXPANDED, "a pin keeps it open after the pointer leaves");
    }

    #[test]
    fn unpinning_gives_the_width_back_at_once_even_while_hovered() {
        let mut r = Rail::new();
        r.hover(true);
        r.toggle_pin();
        settle(&mut r);
        assert_eq!(r.reserved(), EXPANDED);
        r.toggle_pin();
        assert_eq!(r.reserved(), COLLAPSED, "immediately, before the spring retracts");
        assert!(r.width.pos > COLLAPSED);
    }

    #[test]
    fn collapsing_survives_reselecting_the_same_project_and_opens_on_another() {
        let mut r = Rail::new();
        r.select(Some(0));
        r.toggle_collapsed();
        assert!(r.collapsed());
        r.select(Some(0));
        assert!(r.collapsed(), "a refresh that names the same project keeps the fold");
        r.select(Some(1));
        assert!(!r.collapsed(), "another project's list opens");
        r.toggle_collapsed();
        r.select(None);
        assert!(!r.collapsed());
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
