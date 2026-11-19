//! The band's geometry on a rounded outline: where the activity sheen and
//! the done pulse paint, following the border's contour through its corners.
//!
//! The design choice: the band stays a scanline — its brightness depends
//! only on the x fraction, so the top and bottom edges light up together —
//! but it is painted along the outline, not as straight strips: linear
//! ramps on the straight edges, annular sectors through each corner arc,
//! solid runs down the sides. Straight strips clipped by a rounded box gap
//! out at the corners, where the border is neither horizontal nor vertical;
//! these pieces cover exactly the ring the border draws, so the light
//! follows the rounding. Pure geometry in the surface's own pixels; the
//! view supplies the size it laid out and paints the pieces.

use super::sheen::BAND;

/// The border stroke the band rides, in pixels.
pub const EDGE: f32 = 1.5;
/// Paint step along a corner arc, in pixels of arc length.
const STEP: f32 = 1.5;
/// Below this alpha a piece is invisible and not painted.
const MIN: f32 = 1. / 255.;

/// One piece of the band, in the surface's own pixel coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// A horizontal run whose alpha ramps linearly from `a0` at its left
    /// edge to `a1` at its right: the straight top and bottom edges, split
    /// at the band's peak so each run is one linear gradient.
    Ramp { x: f32, y: f32, w: f32, h: f32, a0: f32, a1: f32 },
    /// A solid run: the straight left and right edges, where x barely moves
    /// so the band's alpha is one value along the whole run.
    Solid { x: f32, y: f32, w: f32, h: f32, a: f32 },
    /// One step of a corner arc: an annular sector, four points from the
    /// outer edge around to the inner, alpha sampled at the step's x.
    Sector { points: [(f32, f32); 4], a: f32 },
}

/// The band's alpha multiplier at an x fraction: a triangle peaking at
/// `center`, zero half a band away — the sheen's two linear gradient halves
/// as one function.
pub fn alpha(f: f32, center: f32) -> f32 {
    (1. - (f - center).abs() / (BAND / 2.)).clamp(0., 1.)
}

/// The pieces of the band on a `w`×`h` surface whose corners round by
/// `corners` (top-left, top-right, bottom-right, bottom-left), the band's
/// centre at the `center` x fraction. Pieces tile the border ring without
/// overlap; a band fully off the surface yields none.
pub fn pieces(w: f32, h: f32, corners: [f32; 4], center: f32) -> Vec<Piece> {
    let mut out = Vec::new();
    if w <= 0. || h <= 0. {
        return out;
    }
    let clamp = |r: f32| r.min(w / 2.).min(h / 2.).max(0.);
    let [tl, tr, br, bl] = [clamp(corners[0]), clamp(corners[1]), clamp(corners[2]), clamp(corners[3])];
    // The straight top and bottom edges, between their corner arcs.
    ramp(&mut out, tl, w - tr, 0., center, w);
    ramp(&mut out, bl, w - br, h - EDGE, center, w);
    // The straight left and right edges: one solid run each.
    for (x, top, bottom) in [(0., tl, bl), (w - EDGE, tr, br)] {
        let a = alpha((x + EDGE / 2.) / w, center);
        if a > MIN && h > top + bottom {
            out.push(Piece::Solid { x, y: top, w: EDGE, h: h - top - bottom, a });
        }
    }
    // The corner arcs, as annular sectors, each on its own radius. Angles
    // in screen coordinates (y down): each arc runs from the side edge to
    // the top or bottom edge.
    let pi = std::f32::consts::PI;
    let arcs = [
        ((tl, tl), tl, pi, 1.5 * pi),
        ((w - tr, tr), tr, 1.5 * pi, 2. * pi),
        ((w - br, h - br), br, 0., 0.5 * pi),
        ((bl, h - bl), bl, 0.5 * pi, pi),
    ];
    for ((cx, cy), r, a0, a1) in arcs {
        if r <= 0. {
            continue;
        }
        let arc_r = (r - EDGE / 2.).max(0.);
        let steps = (((a1 - a0) * arc_r) / STEP).ceil().max(1.) as usize;
        for i in 0..steps {
            let t0 = a0 + (a1 - a0) * i as f32 / steps as f32;
            let t1 = a0 + (a1 - a0) * (i + 1) as f32 / steps as f32;
            let a = alpha((cx + arc_r * ((t0 + t1) / 2.).cos()) / w, center);
            if a <= MIN {
                continue;
            }
            let p = |t: f32, rad: f32| (cx + rad * t.cos(), cy + rad * t.sin());
            out.push(Piece::Sector {
                points: [p(t0, r), p(t1, r), p(t1, (r - EDGE).max(0.)), p(t0, (r - EDGE).max(0.))],
                a,
            });
        }
    }
    out
}

/// The straight run from `x0` to `x1` at height `y`, split at the band's
/// peak so each piece's alpha is one linear ramp.
fn ramp(out: &mut Vec<Piece>, x0: f32, x1: f32, y: f32, center: f32, w: f32) {
    if x1 <= x0 {
        return;
    }
    let peak = center * w;
    let mut cuts = vec![x0];
    if peak > x0 && peak < x1 {
        cuts.push(peak);
    }
    cuts.push(x1);
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (a0, a1) = (alpha(a / w, center), alpha(b / w, center));
        if a0.max(a1) > MIN {
            out.push(Piece::Ramp { x: a, y, w: b - a, h: EDGE, a0, a1 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 300.;
    const H: f32 = 30.;
    const R: f32 = 10.;

    #[test]
    fn the_alpha_is_a_triangle_peaking_at_the_centre() {
        assert_eq!(alpha(0.5, 0.5), 1.);
        assert_eq!(alpha(0.5 - BAND / 2., 0.5), 0.);
        assert_eq!(alpha(0.5 + BAND / 2., 0.5), 0.);
        assert!((alpha(0.5 - BAND / 4., 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(alpha(2., 0.5), 0., "far off the row, nothing");
    }

    #[test]
    fn the_straight_edges_are_ramps_split_at_the_peak() {
        let ps = pieces(W, H, [R; 4], 0.5);
        let top: Vec<_> = ps.iter().filter(|p| matches!(p, Piece::Ramp { y, .. } if *y == 0.)).collect();
        assert_eq!(top.len(), 2, "the peak splits the top edge");
        let Piece::Ramp { x, w, a0, a1, .. } = top[0] else { unreachable!() };
        assert_eq!((*x, *w), (R, 0.5 * W - R), "the rising run starts where the arc ends");
        assert!(*a0 < 0.1 && (*a1 - 1.).abs() < 1e-3, "the ramp rises to the peak");
        let Piece::Ramp { x, w, a0, a1, .. } = top[1] else { unreachable!() };
        assert_eq!((*x, *w), (0.5 * W, W - R - 0.5 * W));
        assert!((*a0 - 1.).abs() < 1e-3 && *a1 < 0.1, "the falling run leaves the peak");
        // The bottom edge mirrors the top.
        assert!(ps.iter().any(|p| matches!(p, Piece::Ramp { y, .. } if *y == H - EDGE)));
    }

    #[test]
    fn the_sides_are_solid_runs_between_the_arcs() {
        let ps = pieces(W, H, [R; 4], 0.02);
        let left = ps.iter().find_map(|p| match p {
            Piece::Solid { x, y, w, h, a } if *x == 0. => Some((*y, *w, *h, *a)),
            _ => None,
        });
        assert_eq!(left, Some((R, EDGE, H - 2. * R, alpha(EDGE / 2. / W, 0.02))));
        // The band near the left edge lights the left side, not the right.
        assert!(!ps.iter().any(|p| matches!(p, Piece::Solid { x, .. } if *x == W - EDGE)));
    }

    #[test]
    fn the_corner_sectors_follow_the_arc_and_tile_it() {
        let ps = pieces(W, H, [R; 4], 0.05);
        let sectors: Vec<_> = ps
            .iter()
            .filter_map(|p| match p {
                Piece::Sector { points, a } => Some((points, *a)),
                _ => None,
            })
            .collect();
        assert!(!sectors.is_empty(), "the band near the left edge lights the left corners");
        // Top-left arc: from the left edge (0, R) to the top edge (R, 0),
        // every point on the outer or inner rounding circle about (R, R).
        let tl: Vec<_> = sectors.iter().filter(|(pts, _)| pts[0].0 < R && pts[0].1 <= R + 1e-3).collect();
        assert!(!tl.is_empty());
        let first = tl[0].0;
        assert!((first[0].0 - 0.).abs() < 1e-3 && (first[0].1 - R).abs() < 1e-3, "starts on the left edge");
        let last = tl.last().unwrap().0;
        assert!((last[1].0 - R).abs() < 1e-3 && (last[1].1 - 0.).abs() < 1e-3, "ends on the top edge");
        for (pts, _) in &tl {
            for (i, (x, y)) in pts.iter().enumerate() {
                let rad = if i < 2 { R } else { R - EDGE };
                let d = ((x - R).powi(2) + (y - R).powi(2)).sqrt();
                assert!((d - rad).abs() < 1e-3, "point {i} on its circle: {d} vs {rad}");
            }
        }
        // Consecutive sectors share their radial edge: no gap, no overlap.
        for pair in tl.windows(2) {
            assert_eq!(pair[0].0[1], pair[1].0[0]);
            assert_eq!(pair[0].0[2], pair[1].0[3]);
        }
    }

    #[test]
    fn a_band_off_the_row_paints_nothing() {
        assert!(pieces(W, H, [R; 4], 2.).is_empty());
        assert!(pieces(W, H, [R; 4], -1.).is_empty());
    }

    #[test]
    fn each_corner_keeps_its_own_radius() {
        // A user bubble: round but for a tight bottom-right corner.
        let ps = pieces(W, H, [12., 12., 6., 12.], 0.95);
        // The bottom edge runs from the bottom-left arc to the tight one.
        let bottom: Vec<_> = ps.iter().filter(|p| matches!(p, Piece::Ramp { y, .. } if *y == H - EDGE)).collect();
        let (first, last) = (bottom.first().unwrap(), bottom.last().unwrap());
        let Piece::Ramp { x, .. } = first else { unreachable!() };
        assert!(*x >= 12., "the bottom run stays between its arcs");
        let Piece::Ramp { x, w, .. } = last else { unreachable!() };
        assert_eq!(*x + *w, W - 6.);
        // The right side runs between the two right arcs.
        let right = ps.iter().find_map(|p| match p {
            Piece::Solid { x, y, h, .. } if *x == W - EDGE => Some((*y, *h)),
            _ => None,
        });
        assert_eq!(right, Some((12., H - 12. - 6.)));
        // Bottom-right sectors sit on the tight circle about (W-6, H-6).
        let br: Vec<_> = ps
            .iter()
            .filter_map(|p| match p {
                Piece::Sector { points, .. } if points[0].0 > W - 12. && points[0].1 > H - 12. => Some(points),
                _ => None,
            })
            .collect();
        assert!(!br.is_empty(), "the band near the right edge lights the tight corner");
        for pts in br {
            for (i, (x, y)) in pts.iter().enumerate() {
                let rad = if i < 2 { 6. } else { 6. - EDGE };
                let d = ((x - (W - 6.)).powi(2) + (y - (H - 6.)).powi(2)).sqrt();
                assert!((d - rad).abs() < 1e-3, "point {i} on the tight circle: {d} vs {rad}");
            }
        }
    }

    #[test]
    fn the_radius_clamps_to_a_small_surface() {
        let ps = pieces(10., 10., [10.; 4], 0.5);
        assert!(!ps.is_empty());
        for p in &ps {
            match p {
                Piece::Sector { points, .. } => {
                    for (x, y) in points {
                        assert!(*x >= -1e-3 && *x <= 10. + 1e-3 && *y >= -1e-3 && *y <= 10. + 1e-3);
                    }
                }
                Piece::Ramp { x, w, .. } => assert!(*x >= 0. && *x + *w <= 10. + 1e-3),
                Piece::Solid { x, w, .. } => assert!(*x >= 0. && *x + *w <= 10. + 1e-3),
            }
        }
    }
}
