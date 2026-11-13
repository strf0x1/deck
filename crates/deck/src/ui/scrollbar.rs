// Portions ported from gpui-kit v0.9.1, crates/base/src/scrollbar.rs
// (`ThumbGeometry`, the drag grab point and track-click centring).
// Copyright 2024 - 2026 Longbridge <https://longbridge.com>.
// Licensed under the Apache License, Version 2.0:
// http://www.apache.org/licenses/LICENSE-2.0
// Changed for deck: plain f32 instead of GPUI pixels, positive scroll tops,
// vertical only, and visibility driven by deck's springs instead of tweens.

//! The transcript scrollbar as pure state: where the thumb sits for a list's
//! offset, which scroll top a drag or a track click asks for, and when the
//! bar shows.
//!
//! The design choice: one geometry serves painting, track clicks and
//! dragging, so the thumb never drifts from the pointer that holds it. The
//! bar is overlay glass that stays hidden until the list scrolls or the
//! pointer reaches its strip, then fades out after `IDLE`; fade and the
//! hover widening are springs (`spring.rs`), never tweens.

use super::spring::Spring;
use std::time::{Duration, Instant};

/// How long the bar stays after the last scroll or hover.
pub const IDLE: Duration = Duration::from_millis(1200);
/// The thumb's width at rest and while hovered or held.
pub const THIN: f32 = 4.;
pub const WIDE: f32 = 8.;
/// The shortest thumb, so a long session still has something to grab.
pub const MIN_THUMB: f32 = 36.;
/// A spring's span: 0 hidden (or thin), 100 shown (or wide).
const FULL: f32 = 100.;

/// The thumb along the track. `length` excludes the inset at both ends;
/// `travel` is how far the whole thumb can move; `extent` is how far the
/// content can scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    origin: f32,
    inset: f32,
    pub length: f32,
    travel: f32,
    extent: f32,
}

impl Geometry {
    /// The thumb for a `viewport` showing part of `content`, on a track
    /// that starts at `origin` and is `track` long. `None` when everything
    /// fits and there is nothing to scroll.
    pub fn new(origin: f32, track: f32, viewport: f32, content: f32, inset: f32) -> Option<Self> {
        if content <= viewport + 0.5 || track <= 0. {
            return None;
        }
        let logical = (viewport / content * track).max(MIN_THUMB).min(track);
        let inset = inset.clamp(0., logical / 2.);
        Some(Geometry {
            origin,
            inset,
            length: logical - inset * 2.,
            travel: track - logical,
            extent: content - viewport,
        })
    }

    /// Where the thumb starts for a list scrolled `top` from its start.
    pub fn start(self, top: f32) -> f32 {
        self.origin + self.inset + (top / self.extent).clamp(0., 1.) * self.travel
    }

    /// The scroll top that puts the point `grab` into the thumb under the
    /// pointer at `position`.
    pub fn top_at(self, position: f32, grab: f32) -> f32 {
        if self.travel <= 0. {
            return 0.;
        }
        self.extent * ((position - self.origin - self.inset - grab) / self.travel).clamp(0., 1.)
    }

    /// The scroll top for a click on the track: centre the thumb there.
    pub fn top_for_click(self, position: f32) -> f32 {
        self.top_at(position, self.length / 2.)
    }

    /// Whether `top` is the end of the content.
    pub fn at_end(self, top: f32) -> bool {
        top >= self.extent - 1.
    }
}

/// What the pointer and the list have done to the bar, and its two springs.
#[derive(Debug)]
pub struct Bar {
    shown: Spring,
    widened: Spring,
    last_top: Option<f32>,
    active_at: Option<Instant>,
    /// The pointer is over the bar's strip.
    pub hovered: bool,
    /// The pointer is over the thumb itself.
    pub on_thumb: bool,
    /// Where in the thumb the pointer grabbed it, while held.
    pub grab: Option<f32>,
    last_frame: Option<Instant>,
    /// A wake-up for the end of `IDLE` is already pending.
    pub idle_timer: bool,
}

impl Default for Bar {
    fn default() -> Self {
        Bar {
            shown: Spring::at(0.),
            widened: Spring::at(0.),
            last_top: None,
            active_at: None,
            hovered: false,
            on_thumb: false,
            grab: None,
            last_frame: None,
            idle_timer: false,
        }
    }
}

impl Bar {
    /// Note the list's scroll top; a change counts as activity. The first
    /// observation does not, so opening a session does not flash the bar.
    pub fn observe(&mut self, top: f32, now: Instant) {
        if self.last_top.is_some_and(|last| (last - top).abs() > 0.5) {
            self.active_at = Some(now);
        }
        self.last_top = Some(top);
    }

    /// Hover or a press keeps the bar alive; record it as activity too, so
    /// the bar waits `IDLE` after the pointer leaves.
    pub fn touch(&mut self, now: Instant) {
        self.active_at = Some(now);
    }

    fn wants_shown(&self, now: Instant) -> bool {
        self.grab.is_some() || self.hovered || self.active_at.is_some_and(|t| now.duration_since(t) < IDLE)
    }

    /// How long until the bar should start fading, while it is waiting to.
    pub fn idle_left(&self, now: Instant) -> Option<Duration> {
        if self.grab.is_some() || self.hovered {
            return None;
        }
        let since = now.duration_since(self.active_at?);
        IDLE.checked_sub(since).filter(|d| !d.is_zero())
    }

    /// Aim and advance both springs to `now`; whether either still moves.
    pub fn step(&mut self, now: Instant) -> bool {
        let dt = self.last_frame.map_or(0., |t| now.duration_since(t).as_secs_f32());
        self.last_frame = Some(now);
        self.shown.target = if self.wants_shown(now) { FULL } else { 0. };
        self.widened.target = if self.grab.is_some() || self.on_thumb { FULL } else { 0. };
        self.shown.step(dt);
        self.widened.step(dt);
        let moving = !self.shown.at_rest() || !self.widened.at_rest();
        if !moving {
            self.last_frame = None;
        }
        moving
    }

    /// How shown the bar is, 0 to 1.
    pub fn opacity(&self) -> f32 {
        (self.shown.pos / FULL).clamp(0., 1.)
    }

    /// The thumb's painted width.
    pub fn width(&self) -> f32 {
        THIN + (WIDE - THIN) * (self.widened.pos / FULL).clamp(0., 1.)
    }

    /// Whether the bar takes the pointer at all: a hidden bar lets clicks
    /// through to the cards under it.
    pub fn live(&self) -> bool {
        self.opacity() > 0.05 || self.hovered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        // A 400px track over a 400px viewport of 2000px content.
        Geometry::new(0., 400., 400., 2000., 0.).expect("scrollable")
    }

    #[test]
    fn nothing_to_scroll_has_no_thumb() {
        assert_eq!(Geometry::new(0., 400., 400., 400., 0.), None);
        assert_eq!(Geometry::new(0., 400., 400., 300., 0.), None);
    }

    #[test]
    fn the_thumb_is_proportional_and_has_a_floor() {
        assert_eq!(geometry().length, 80.);
        let long = Geometry::new(0., 400., 400., 400_000., 0.).expect("scrollable");
        assert_eq!(long.length, MIN_THUMB);
    }

    #[test]
    fn the_thumb_travels_the_whole_track() {
        let g = geometry();
        assert_eq!(g.start(0.), 0.);
        assert_eq!(g.start(1600.), 320.);
        assert_eq!(g.start(99_999.), 320., "clamped past the end");
    }

    #[test]
    fn a_drag_keeps_the_grab_point_under_the_pointer() {
        let g = geometry();
        // Grabbed 20px into the thumb, which starts at 160 for top 800.
        let top = 800.;
        let grab = 20.;
        assert_eq!(g.top_at(g.start(top) + grab, grab), top);
    }

    #[test]
    fn a_track_click_centres_the_thumb() {
        let g = geometry();
        let top = g.top_for_click(200.);
        assert_eq!(g.start(top) + g.length / 2., 200.);
        assert!(g.at_end(g.top_for_click(400.)));
    }

    #[test]
    fn scrolling_shows_the_bar_and_idle_hides_it() {
        let t0 = Instant::now();
        let mut bar = Bar::default();
        bar.observe(0., t0);
        assert!(!bar.step(t0), "the first observation is not a scroll");
        assert_eq!(bar.opacity(), 0.);

        bar.observe(120., t0);
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            bar.step(t);
        }
        assert_eq!(bar.opacity(), 1.);
        assert!(bar.idle_left(t).is_some());

        let late = t0 + IDLE + Duration::from_millis(1);
        assert_eq!(bar.idle_left(late), None);
        let mut t = late;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            bar.step(t);
        }
        assert_eq!(bar.opacity(), 0.);
    }

    #[test]
    fn hovering_the_thumb_widens_it_and_holds_the_bar() {
        let t0 = Instant::now();
        let mut bar = Bar { hovered: true, on_thumb: true, ..Bar::default() };
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_millis(16);
            bar.step(t);
        }
        assert_eq!(bar.width(), WIDE);
        assert_eq!(bar.idle_left(t + IDLE * 2), None, "hover never times out");
        assert!(bar.live());
    }
}
