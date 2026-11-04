// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable.cpp, filegdbtable_write.cpp

//! Planar measures and predicates on [`Shape`]s, exact for circular arcs.
//!
//! A ring with circular arcs is its "chord polygon" (the vertices joined by
//! straight lines) plus or minus, for each arc, the circular segment between
//! the chord and the arc. Areas, lengths and point-in-polygon tests are
//! computed that way, so circles (e.g. Esri buffers stored as two-vertex true
//! curves) are measured exactly rather than approximated.
//!
//! Bezier and elliptic segments are approximated by densification (Bezier with
//! GDAL's heuristic; ellipses as straight chords, which are rare in practice).

use crate::{Coord, CurveSegment, Shape, ShapeType};
use std::f64::consts::PI;

/// A circular arc in normalised form.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arc {
    pub(crate) cx: f64,
    pub(crate) cy: f64,
    pub(crate) r: f64,
    /// Angle of the start point.
    pub(crate) a0: f64,
    /// Signed sweep, counter-clockwise positive.
    pub(crate) sweep: f64,
}

impl Arc {
    /// The arc from `p1` to `p3` described by `seg`.
    fn from_segment(p1: Coord, p3: Coord, seg: &CurveSegment) -> Option<Arc> {
        match *seg {
            CurveSegment::ArcCenterPoint { center_x, center_y, ccw, .. } => {
                let a0 = (p1.y - center_y).atan2(p1.x - center_x);
                let mut a1 = (p3.y - center_y).atan2(p3.x - center_x);
                if ccw {
                    if a0 >= a1 {
                        a1 += 2.0 * PI;
                    }
                } else if a0 <= a1 {
                    a1 -= 2.0 * PI;
                }
                let r = (p1.x - center_x).hypot(p1.y - center_y);
                Some(Arc { cx: center_x, cy: center_y, r, a0, sweep: a1 - a0 })
            }
            CurveSegment::ArcInteriorPoint { x, y, ccw, .. } => {
                if p1.x == p3.x && p1.y == p3.y {
                    // Full circle: the interior point is diametrically opposite.
                    let cx = (p1.x + x) / 2.0;
                    let cy = (p1.y + y) / 2.0;
                    let r = (p1.x - cx).hypot(p1.y - cy);
                    let a0 = (p1.y - cy).atan2(p1.x - cx);
                    return Some(Arc { cx, cy, r, a0, sweep: if ccw { 2.0 * PI } else { -2.0 * PI } });
                }
                circle_through(p1.x, p1.y, x, y, p3.x, p3.y)
            }
            _ => None,
        }
    }

    /// Area between the arc and its chord, signed like the shoelace formula
    /// of a ring traversing the arc (counter-clockwise positive).
    fn segment_signed_area(&self) -> f64 {
        self.r * self.r / 2.0 * (self.sweep - self.sweep.sin())
    }

    fn point_at(&self, t: f64) -> (f64, f64) {
        let a = self.a0 + self.sweep * t;
        (self.cx + self.r * a.cos(), self.cy + self.r * a.sin())
    }

    /// Whether (x, y) lies in the region between the chord and the arc.
    fn segment_contains(&self, p1: Coord, p3: Coord, x: f64, y: f64) -> bool {
        let d2 = (x - self.cx).powi(2) + (y - self.cy).powi(2);
        if d2 > self.r * self.r {
            return false;
        }
        if self.sweep.abs() >= 2.0 * PI - 1e-12 {
            return true;
        }
        let (mx, my) = self.point_at(0.5);
        let side = |px: f64, py: f64| (p3.x - p1.x) * (py - p1.y) - (p3.y - p1.y) * (px - p1.x);
        let sp = side(x, y);
        let sm = side(mx, my);
        (sp >= 0.0) == (sm >= 0.0)
    }
}

/// The arc from (x1, y1) through (x2, y2) to (x3, y3).
fn circle_through(x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) -> Option<Arc> {
    // Work relative to the first point for precision.
    let (bx, by) = (x2 - x1, y2 - y1);
    let (cx, cy) = (x3 - x1, y3 - y1);
    let d = 2.0 * (bx * cy - by * cx);
    if d == 0.0 {
        return None; // Collinear: a straight segment.
    }
    let b2 = bx * bx + by * by;
    let c2 = cx * cx + cy * cy;
    let ux = (cy * b2 - by * c2) / d;
    let uy = (bx * c2 - cx * b2) / d;
    let (ox, oy) = (x1 + ux, y1 + uy);
    let r = ux.hypot(uy);
    let a0 = (y1 - oy).atan2(x1 - ox);
    let a2 = (y2 - oy).atan2(x2 - ox);
    let a3 = (y3 - oy).atan2(x3 - ox);
    let ccw = d > 0.0;
    let norm = |a: f64| {
        let mut a = a;
        while a < 0.0 {
            a += 2.0 * PI;
        }
        while a >= 2.0 * PI {
            a -= 2.0 * PI;
        }
        a
    };
    let sweep = if ccw { norm(a3 - a0) } else { -norm(a0 - a3) };
    let _ = a2;
    Some(Arc { cx: ox, cy: oy, r, a0, sweep })
}

/// A part with its global start index into the concatenated point list.
fn part_ranges(shape: &Shape) -> Vec<(usize, &[Coord])> {
    let mut out = Vec::with_capacity(shape.parts.len());
    let mut start = 0;
    for p in &shape.parts {
        out.push((start, p.as_slice()));
        start += p.len();
    }
    out
}

/// Curves keyed by global start index.
fn curve_at(shape: &Shape, global: usize) -> Option<&CurveSegment> {
    shape.curves.iter().find(|c| c.start_index() == global)
}

/// Signed shoelace area of a ring (counter-clockwise positive), relative to the
/// first vertex to limit cancellation with large state-plane coordinates.
pub(crate) fn ring_signed_area(ring: &[Coord]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let (x0, y0) = (ring[0].x, ring[0].y);
    let n = ring.len();
    let mut s = 0.0;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        s += (a.x - x0) * (b.y - y0) - (b.x - x0) * (a.y - y0);
    }
    s / 2.0
}

fn bezier_points(p0: Coord, p3: Coord, x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<(f64, f64)> {
    // GDAL's step heuristic (OGRGeometryFactory default arc step: 4 degrees).
    let mut sa = (y1 - p0.y).atan2(x1 - p0.x);
    let mut ea = (p3.y - y2).atan2(p3.x - x2);
    if sa + PI < ea {
        sa += 2.0 * PI;
    } else if ea + PI < sa {
        ea += 2.0 * PI;
    }
    let step = 4.0_f64.to_radians();
    let lts = (x1 - p0.x).powi(2) + (y1 - p0.y).powi(2);
    let lte = (p3.x - x2).powi(2) + (p3.y - y2).powi(2);
    let len = (p3.x - p0.x).powi(2) + (p3.y - p0.y).powi(2);
    let n =
        if len < 1e-9 { 1 } else { (((ea - sa).abs() / step).max(2.0) * (5.0 * (lts + lte) / len).max(1.0)).ceil().min(1000.0) as usize };
    (1..n)
        .map(|j| {
            let t = j as f64 / n as f64;
            let u = 1.0 - t;
            (
                u * u * u * p0.x + 3.0 * u * u * t * x1 + 3.0 * u * t * t * x2 + t * t * t * p3.x,
                u * u * u * p0.y + 3.0 * u * u * t * y1 + 3.0 * u * t * t * y2 + t * t * t * p3.y,
            )
        })
        .collect()
}

impl Shape {
    /// Planar area. Polygons only (0 otherwise). Follows Esri orientation:
    /// clockwise rings (exteriors) count positive, counter-clockwise rings
    /// (holes) negative. Exact for circular arcs.
    pub fn area(&self) -> f64 {
        if self.shape_type != ShapeType::Polygon {
            return 0.0;
        }
        if !self.has_curves || self.curves.iter().any(|c| matches!(c, CurveSegment::Bezier { .. } | CurveSegment::Ellipse { .. })) {
            let s = if self.has_curves { self.densify(1.0) } else { self.clone() };
            return -s.parts.iter().map(|r| ring_signed_area(r)).sum::<f64>();
        }
        let mut total = 0.0;
        for (start, ring) in part_ranges(self) {
            let mut a = ring_signed_area(ring);
            for i in 0..ring.len().saturating_sub(1) {
                if let Some(seg) = curve_at(self, start + i)
                    && let Some(arc) = Arc::from_segment(ring[i], ring[i + 1], seg)
                {
                    a += arc.segment_signed_area();
                }
            }
            total -= a;
        }
        total
    }

    /// Planar length (sum of segment lengths; perimeter for polygons). Exact
    /// for circular arcs.
    pub fn length(&self) -> f64 {
        let mut total = 0.0;
        for (start, p) in part_ranges(self) {
            for i in 0..p.len().saturating_sub(1) {
                let chord = (p[i + 1].x - p[i].x).hypot(p[i + 1].y - p[i].y);
                match curve_at(self, start + i) {
                    Some(seg @ (CurveSegment::ArcCenterPoint { .. } | CurveSegment::ArcInteriorPoint { .. })) => {
                        total += Arc::from_segment(p[i], p[i + 1], seg).map_or(chord, |a| a.r * a.sweep.abs());
                    }
                    Some(CurveSegment::Bezier { x1, y1, x2, y2, .. }) => {
                        let mut pts = vec![(p[i].x, p[i].y)];
                        pts.extend(bezier_points(p[i], p[i + 1], *x1, *y1, *x2, *y2));
                        pts.push((p[i + 1].x, p[i + 1].y));
                        total += pts.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).sum::<f64>();
                    }
                    _ => total += chord,
                }
            }
        }
        total
    }

    /// Envelope (xmin, ymin, xmax, ymax) including arc bulges; `None` if empty.
    pub fn envelope(&self) -> Option<[f64; 4]> {
        let s = if self.has_curves { self.densify(0.5) } else { self.clone() };
        let mut it = s.parts.iter().flatten();
        let first = it.next()?;
        let mut e = [first.x, first.y, first.x, first.y];
        for c in it {
            e[0] = e[0].min(c.x);
            e[1] = e[1].min(c.y);
            e[2] = e[2].max(c.x);
            e[3] = e[3].max(c.y);
        }
        Some(e)
    }

    /// Whether the polygon contains (x, y) (even-odd rule over all rings).
    /// Exact for circular arcs; points exactly on the boundary may go either way.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        if self.shape_type != ShapeType::Polygon {
            return false;
        }
        if self.curves.iter().any(|c| matches!(c, CurveSegment::Bezier { .. } | CurveSegment::Ellipse { .. })) {
            return self.densify(0.25).contains_point(x, y);
        }
        let mut inside = false;
        for (start, ring) in part_ranges(self) {
            let n = ring.len();
            if n == 0 {
                continue;
            }
            // Crossing number on the chord polygon.
            let mut j = n - 1;
            for i in 0..n {
                let (a, b) = (ring[i], ring[j]);
                if (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x {
                    inside = !inside;
                }
                j = i;
            }
            // Circular segments toggle membership.
            for i in 0..n.saturating_sub(1) {
                if let Some(seg) = curve_at(self, start + i)
                    && let Some(arc) = Arc::from_segment(ring[i], ring[i + 1], seg)
                    && arc.segment_contains(ring[i], ring[i + 1], x, y)
                {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// A copy with curve segments replaced by vertices at most `max_step_deg`
    /// degrees apart (arcs) or GDAL's Bezier steps. Straight shapes are copied.
    pub fn densify(&self, max_step_deg: f64) -> Shape {
        if !self.has_curves {
            return self.clone();
        }
        let step = max_step_deg.max(1e-3).to_radians();
        let mut parts = Vec::with_capacity(self.parts.len());
        for (start, p) in part_ranges(self) {
            let mut out = Vec::with_capacity(p.len());
            for i in 0..p.len() {
                out.push(p[i]);
                if i + 1 >= p.len() {
                    break;
                }
                let interp = |x: f64, y: f64| Coord { x, y, z: p[i].z, m: p[i].m };
                match curve_at(self, start + i) {
                    Some(seg @ (CurveSegment::ArcCenterPoint { .. } | CurveSegment::ArcInteriorPoint { .. })) => {
                        if let Some(arc) = Arc::from_segment(p[i], p[i + 1], seg) {
                            let n = ((arc.sweep.abs() / step).ceil() as usize).max(2);
                            for k in 1..n {
                                let (x, y) = arc.point_at(k as f64 / n as f64);
                                out.push(interp(x, y));
                            }
                        }
                    }
                    Some(CurveSegment::Bezier { x1, y1, x2, y2, .. }) => {
                        for (x, y) in bezier_points(p[i], p[i + 1], *x1, *y1, *x2, *y2) {
                            out.push(interp(x, y));
                        }
                    }
                    _ => {}
                }
            }
            parts.push(out);
        }
        Shape { shape_type: self.shape_type, parts, has_curves: false, curves: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(cx: f64, cy: f64, r: f64, ccw: bool) -> Shape {
        let p = Coord::xy(cx, cy + r);
        Shape {
            shape_type: ShapeType::Polygon,
            parts: vec![vec![p, p]],
            has_curves: true,
            curves: vec![CurveSegment::ArcInteriorPoint { start_index: 0, x: cx, y: cy - r, ccw }],
        }
    }

    #[test]
    fn full_circle_area_and_contains() {
        let s = circle(3_000_000.0, 13_800_000.0, 10.0, false);
        assert!((s.area() - PI * 100.0).abs() < 1e-6, "{}", s.area());
        assert!((s.length() - 2.0 * PI * 10.0).abs() < 1e-9);
        assert!(s.contains_point(3_000_000.0, 13_800_000.0));
        assert!(s.contains_point(3_000_009.9, 13_800_000.0));
        assert!(!s.contains_point(3_000_010.1, 13_800_000.0));
        assert!(!s.contains_point(3_000_007.2, 13_800_007.2));
        let d = s.densify(1.0);
        assert!((d.area() - PI * 100.0).abs() < 0.1);
    }

    #[test]
    fn half_disk() {
        // Clockwise ring: (1,0) -> arc through (0,1) -> (-1,0) -> back to (1,0).
        // That traversal is counter-clockwise, so the Esri area is negative.
        let s = Shape {
            shape_type: ShapeType::Polygon,
            parts: vec![vec![Coord::xy(1.0, 0.0), Coord::xy(-1.0, 0.0), Coord::xy(1.0, 0.0)]],
            has_curves: true,
            curves: vec![CurveSegment::ArcInteriorPoint { start_index: 0, x: 0.0, y: 1.0, ccw: false }],
        };
        assert!((s.area() + PI / 2.0).abs() < 1e-12);
        assert!(s.contains_point(0.0, 0.5));
        assert!(!s.contains_point(0.0, -0.5));
        assert!((s.length() - (PI + 2.0)).abs() < 1e-12);
    }

    #[test]
    fn center_point_arc_matches_interior_point() {
        let p1 = Coord::xy(1.0, 0.0);
        let p3 = Coord::xy(0.0, 1.0);
        let a =
            Arc::from_segment(p1, p3, &CurveSegment::ArcCenterPoint { start_index: 0, center_x: 0.0, center_y: 0.0, ccw: true }).unwrap();
        let h = std::f64::consts::FRAC_1_SQRT_2;
        let b = Arc::from_segment(p1, p3, &CurveSegment::ArcInteriorPoint { start_index: 0, x: h, y: h, ccw: false }).unwrap();
        assert!((a.sweep - PI / 2.0).abs() < 1e-12);
        assert!((b.sweep - PI / 2.0).abs() < 1e-12);
        assert!((a.r - b.r).abs() < 1e-12);
    }
}
