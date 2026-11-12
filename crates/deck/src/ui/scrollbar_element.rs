// Portions ported from gpui-kit v0.9.1, crates/base/src/scrollbar.rs
// (the `ListState` handle, the overlay element's hitboxes and its mouse
// handling). Copyright 2024 - 2026 Longbridge <https://longbridge.com>.
// Licensed under the Apache License, Version 2.0:
// http://www.apache.org/licenses/LICENSE-2.0
// Changed for deck: vertical only, one `ListState`, deck's theme tokens and
// springs, no theme registry, touch dragging left out (the Zed pin has no
// `TouchDragEvent`).

//! The transcript scrollbar as a GPUI element: an overlay that fills its
//! parent, paints a thumb at the right edge from `scrollbar.rs`, and drags
//! or jumps the list.
//!
//! The design choice: it reads and writes the list only through
//! `ListState`'s scrollbar methods, which freeze the content height while a
//! drag is in progress, so rows measured mid-drag do not shove the thumb
//! from under the pointer. Letting go at the very end hands the list back
//! to following new output.

use super::scrollbar::{Bar, Geometry};
use super::theme::{Tokens, radius};
use gpui::{
    App, Bounds, ContentMask, CursorStyle, Element, ElementId, FollowMode, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, ListState, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Position, Style, Window, fill, point, px, relative, size,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

/// The strip at the right edge that takes the pointer.
const STRIP: f32 = 14.;
/// The thumb's gap from the right edge, and from the pane's top and bottom.
const EDGE: f32 = 3.;
const ENDS: f32 = 6.;

/// A scrollbar over `list`; place it as the last child of a `relative`
/// container the same size as the list.
pub fn scrollbar(list: &ListState) -> Scrollbar {
    Scrollbar { list: list.clone() }
}

pub struct Scrollbar {
    list: ListState,
}

type Shared = Rc<RefCell<Bar>>;

pub struct Prepaint {
    bar: Shared,
    track: Option<Track>,
}

/// One frame's thumb: its geometry and where it is painted.
struct Track {
    geometry: Geometry,
    strip: Hitbox,
    thumb: Bounds<Pixels>,
}

impl Scrollbar {
    /// How far the list is scrolled, and how tall its content is.
    fn measure(&self) -> (f32, f32) {
        let top = -f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        let content = self.list.viewport_bounds().size.height + self.list.max_offset_for_scrollbar().y;
        (top.max(0.), f32::from(content))
    }

    fn scroll_to(&self, top: f32) {
        self.list.set_offset_from_scrollbar(point(px(0.), px(-top)));
    }
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        Some("transcript-scrollbar".into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style { position: Position::Absolute, ..Style::default() };
        style.inset.top = px(0.).into();
        style.inset.left = px(0.).into();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let bar: Shared = window.use_state(cx, |_, _| Shared::default()).read(cx).clone();
        let now = Instant::now();
        let (top, content) = self.measure();
        let viewport = f32::from(bounds.size.height);
        let origin = f32::from(bounds.origin.y) + ENDS;
        let geometry = Geometry::new(origin, viewport - ENDS * 2., viewport, content, 0.);

        let mut state = bar.borrow_mut();
        state.observe(top, now);
        if geometry.is_none() {
            state.hovered = false;
            state.on_thumb = false;
        }
        if state.step(now) {
            window.request_animation_frame();
        }
        if let Some(wait) = state.idle_left(now).filter(|_| !state.idle_timer) {
            state.idle_timer = true;
            let bar = bar.clone();
            let view = window.current_view();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().timer(wait).await;
                    bar.borrow_mut().idle_timer = false;
                    cx.update(|_, cx| cx.notify(view)).ok();
                })
                .detach();
        }
        let width = state.width();
        drop(state);

        let track = geometry.map(|geometry| {
            let strip =
                Bounds::new(point(bounds.right() - px(STRIP), bounds.origin.y), size(px(STRIP), bounds.size.height));
            let strip = window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.insert_hitbox(strip, HitboxBehavior::Normal)
            });
            let thumb = Bounds::new(
                point(bounds.right() - px(EDGE + width), px(geometry.start(top))),
                size(px(width), px(geometry.length)),
            );
            Track { geometry, strip, thumb }
        });
        Prepaint { bar, track }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        _: &mut App,
    ) {
        let Some(track) = prepaint.track.as_ref() else { return };
        let bar = prepaint.bar.clone();
        let (opacity, live, active) = {
            let b = bar.borrow();
            (b.opacity(), b.live(), b.grab.is_some() || b.on_thumb)
        };
        let view = window.current_view();
        let (geometry, thumb, strip) = (track.geometry, track.thumb, track.strip.clone());

        if live {
            window.set_cursor_style(CursorStyle::Arrow, &strip);
        }
        if opacity > 0. {
            let color = if active { Tokens::thumb_active() } else { Tokens::thumb() };
            let mut color: gpui::Hsla = color.into();
            color.a *= opacity;
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                window.paint_layer(bounds, |window| {
                    let round = (thumb.size.width / 2.).min(px(radius::CHIP));
                    window.paint_quad(fill(thumb, color).corner_radii(round));
                });
            });
        }

        // Press: on the thumb, hold it where grabbed; on the track, jump
        // there. A hidden bar lets the press through to the card beneath.
        window.on_mouse_event({
            let (bar, list, strip) = (bar.clone(), self.list.clone(), strip.clone());
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || !live || !strip.is_hovered(window) {
                    return;
                }
                cx.stop_propagation();
                let y = f32::from(event.position.y);
                let mut b = bar.borrow_mut();
                b.touch(Instant::now());
                if thumb.contains(&event.position) {
                    b.grab = Some(y - f32::from(thumb.origin.y));
                    list.scrollbar_drag_started();
                } else {
                    Scrollbar { list: list.clone() }.scroll_to(geometry.top_for_click(y));
                }
                cx.notify(view);
            }
        });

        // Move: track hover, and follow the pointer while held.
        window.on_mouse_event({
            let (bar, list) = (bar.clone(), self.list.clone());
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let mut b = bar.borrow_mut();
                let hovered = strip.is_hovered(window);
                let on_thumb = hovered && thumb.contains(&event.position);
                let mut changed = hovered != b.hovered || on_thumb != b.on_thumb;
                if hovered {
                    b.touch(Instant::now());
                }
                (b.hovered, b.on_thumb) = (hovered, on_thumb);
                if let Some(grab) = b.grab.filter(|_| event.dragging()) {
                    cx.stop_propagation();
                    Scrollbar { list: list.clone() }.scroll_to(geometry.top_at(f32::from(event.position.y), grab));
                    changed = true;
                }
                if changed {
                    cx.notify(view);
                }
            }
        });

        // Release: let go, and resume following if dropped at the end.
        window.on_mouse_event({
            let list = self.list.clone();
            move |event: &MouseUpEvent, phase, _, cx| {
                let mut b = bar.borrow_mut();
                let Some(grab) = b.grab.filter(|_| phase.bubble()) else { return };
                let top = geometry.top_at(f32::from(event.position.y), grab);
                Scrollbar { list: list.clone() }.scroll_to(top);
                list.scrollbar_drag_ended();
                if geometry.at_end(top) {
                    list.set_follow_mode(FollowMode::Tail);
                }
                b.grab = None;
                b.touch(Instant::now());
                cx.notify(view);
            }
        });
    }
}
