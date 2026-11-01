//! Angled panels: rectangles with independently cut corners, painted on a
//! `canvas` behind a view's content. GPUI's quads only round corners, so the
//! asymmetric look is a filled path plus a hairline stroke.
//!
//! The design choice: geometry is a pure function of bounds and [`Cuts`]
//! (tested); painting is a thin wrapper, and the element under it keeps
//! normal layout and hit-testing.

use gpui::{
    AnyElement, App, Background, Bounds, Hsla, IntoElement, ParentElement as _, PathBuilder, Pixels, Point,
    Styled as _, Window, canvas, div, point, px,
};

/// How far each corner is cut, in pixels. Zero is a square corner.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cuts {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl Cuts {
    /// Deck's signature: a big cut top-left, a small one bottom-right.
    pub const fn deck(big: f32, small: f32) -> Self {
        Cuts { top_left: big, top_right: 0., bottom_right: small, bottom_left: 0. }
    }
}

/// The outline, clockwise from the top edge. Cuts are clamped so two cuts on
/// one edge never cross.
pub fn outline(b: Bounds<Pixels>, c: Cuts) -> Vec<Point<Pixels>> {
    let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    let max = (w.min(h) / 2.).max(0.);
    let [tl, tr, br, bl] = [c.top_left, c.top_right, c.bottom_right, c.bottom_left].map(|v| v.clamp(0., max));
    let (x1, y1) = (x0 + w, y0 + h);
    let pts = [
        (x0 + tl, y0),
        (x1 - tr, y0),
        (x1, y0 + tr),
        (x1, y1 - br),
        (x1 - br, y1),
        (x0 + bl, y1),
        (x0, y1 - bl),
        (x0, y0 + tl),
    ];
    let mut out: Vec<Point<Pixels>> = Vec::with_capacity(8);
    for (x, y) in pts {
        let p = point(px(x), px(y));
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

/// What to paint: a fill, an edge, and an optional accent tick along the
/// left edge below the top-left cut.
#[derive(Clone, Copy)]
pub struct Skin {
    pub fill: Background,
    pub edge: Hsla,
    pub tick: Option<Hsla>,
}

fn paint(b: Bounds<Pixels>, cuts: Cuts, skin: Skin, window: &mut Window) {
    let pts = outline(b, cuts);
    let Some((first, rest)) = pts.split_first() else { return };
    let mut fill = PathBuilder::fill();
    fill.move_to(*first);
    for p in rest {
        fill.line_to(*p);
    }
    fill.close();
    if let Ok(path) = fill.build() {
        window.paint_path(path, skin.fill);
    }
    let mut stroke = PathBuilder::stroke(px(1.));
    stroke.move_to(*first);
    for p in rest {
        stroke.line_to(*p);
    }
    stroke.close();
    if let Ok(path) = stroke.build() {
        window.paint_path(path, skin.edge);
    }
    if let Some(color) = skin.tick {
        let top = f32::from(b.origin.y) + cuts.top_left + 6.;
        let len = (f32::from(b.size.height) * 0.35).clamp(10., 48.);
        let x = f32::from(b.origin.x) + 1.5;
        let mut tick = PathBuilder::stroke(px(2.));
        tick.move_to(point(px(x), px(top)));
        tick.line_to(point(px(x), px(top + len)));
        if let Ok(path) = tick.build() {
            window.paint_path(path, color);
        }
    }
}

/// A full-size canvas that paints the angled panel; put it first among a
/// `relative()` element's children so content draws on top.
pub fn backdrop(cuts: Cuts, skin: Skin) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            canvas(|_, _, _| {}, move |b, _, window: &mut Window, _: &mut App| paint(b, cuts, skin, window))
                .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn b(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds { origin: point(px(10.), px(20.)), size: size(px(w), px(h)) }
    }

    #[test]
    fn square_corners_give_a_rectangle() {
        let pts = outline(b(100., 50.), Cuts::default());
        let xy: Vec<(f32, f32)> = pts.iter().map(|p| (f32::from(p.x), f32::from(p.y))).collect();
        assert_eq!(xy, vec![(10., 20.), (110., 20.), (110., 70.), (10., 70.)]);
    }

    #[test]
    fn deck_cuts_make_six_points() {
        let pts = outline(b(100., 50.), Cuts::deck(16., 6.));
        assert_eq!(pts.len(), 6);
        assert_eq!((f32::from(pts[0].x), f32::from(pts[0].y)), (26., 20.));
    }

    #[test]
    fn oversized_cuts_are_clamped_to_half_the_short_side() {
        let pts = outline(b(100., 20.), Cuts { top_left: 500., top_right: 500., bottom_right: 0., bottom_left: 0. });
        assert!(pts.iter().all(|p| f32::from(p.x) >= 10. && f32::from(p.x) <= 110.));
        assert_eq!(f32::from(pts[0].x), 20.);
    }
}
