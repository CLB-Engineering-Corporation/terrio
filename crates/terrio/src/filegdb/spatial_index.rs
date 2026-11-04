// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2022, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbindex_write.cpp

//! Writing of `.spx` spatial indexes and `.gdbindexes` index registries.
//!
//! Ported from GDAL's OpenFileGDB driver (`filegdbindex_write.cpp`:
//! `CreateGdbIndexesFile`, `ComputeOptimalSpatialIndexGridResolution`,
//! `CreateSpatialIndex`, `WriteIndex`; MIT licence, Copyright (c) 2022 Even
//! Rouault). ArcGIS Pro writes the same layout: one grid level, cells of
//! `grid` feet keyed `(level << 62) | (x << 31) | y` with
//! `x = floor(X / grid + 2^29)`, a B-tree of 4096-byte pages.
//!
//! ArcGIS's `CheckGeometry` reports "could not find spatial index" for a
//! feature class without a `.spx`, so the writer always creates one (an empty
//! one, with grid size 0, for an empty feature class, as ArcGIS does).

use crate::filegdb::{Shape, ShapeType};

const PAGE: usize = 4096;
/// Features per page for 8-byte values: (4096 - 12) / (4 + 8).
const PER_PAGE: usize = 340;
const OFFSET_FIRST_VAL: usize = 12 + PER_PAGE * 4;

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_utf16_with_len(b: &mut Vec<u8>, s: &str) {
    let u: Vec<u16> = s.encode_utf16().collect();
    put_u32(b, u.len() as u32);
    for c in u {
        b.extend_from_slice(&c.to_le_bytes());
    }
}

/// `.gdbindexes` content registering `FDO_OBJECTID` (on `oid_field`) and,
/// for feature classes, `FDO_Shape` (on `geom_field`), as ArcGIS writes it.
pub(crate) fn gdbindexes(oid_field: &str, geom_field: Option<&str>) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, 1 + u32::from(geom_field.is_some()));
    let mut entry = |name: &str, expr: &str, kind: u32, flag: u16| {
        put_utf16_with_len(&mut b, name);
        b.extend_from_slice(&0u16.to_le_bytes());
        put_u32(&mut b, kind);
        b.extend_from_slice(&flag.to_le_bytes());
        put_u32(&mut b, 1);
        put_utf16_with_len(&mut b, expr);
        b.extend_from_slice(&0u16.to_le_bytes());
    };
    entry("FDO_OBJECTID", oid_field, 16, 0xFFFF);
    if let Some(g) = geom_field {
        entry("FDO_Shape", g, 4, 0);
    }
    b
}

/// The grid size GDAL computes (`ComputeOptimalSpatialIndexGridResolution`):
/// for points, the side of the mean area per point of the layer extent; for
/// other types, the largest width or height of a feature envelope. 0 if there
/// are no non-empty geometries.
pub(crate) fn grid_size(shape_type: ShapeType, shapes: &[(i64, Shape)], extent: [f64; 4]) -> f64 {
    let n_points: usize = match shape_type {
        ShapeType::Point => shapes.len(),
        ShapeType::Multipoint => shapes.iter().map(|(_, s)| s.parts.iter().map(Vec::len).sum::<usize>()).sum(),
        _ => {
            let mut m = 0.0f64;
            for (_, s) in shapes {
                if let Some(e) = s.envelope() {
                    m = m.max(e[2] - e[0]).max(e[3] - e[1]);
                }
            }
            return m;
        }
    };
    if n_points == 0 || extent[0].is_nan() {
        return 0.0;
    }
    let (w, h) = (extent[2] - extent[0], extent[3] - extent[1]);
    let n = n_points as f64;
    if w * h != 0.0 {
        (w * h / n).sqrt()
    } else if w > 0.0 {
        w / n
    } else if h > 0.0 {
        h / n
    } else {
        0.0
    }
}

struct Burner {
    grid: f64,
    shift: f64,
    out: Vec<i64>,
}

impl Burner {
    fn cell(&mut self, ix: i32, iy: i32) {
        // level 0 (single grid)
        let v = ((ix as u32 as u64) << 31) | (iy as u32 as u64);
        self.out.push(v as i64);
    }

    fn tx(&self, v: f64) -> f64 {
        v / self.grid + self.shift
    }

    fn point(&mut self, x: f64, y: f64) {
        let (sx, sy) = (self.tx(x), self.tx(y));
        if sx.abs() < f64::from(1 << 30) && sy.abs() < f64::from(1 << 30) {
            self.cell(sx.floor() as i32, sy.floor() as i32);
        }
    }

    /// `AddLineStringToIndex` (all-touched line rasterisation).
    fn line(&mut self, pts: &[(f64, f64)]) {
        let lim = f64::from(1 << 30);
        for w in pts.windows(2) {
            let (mut x, mut y) = (self.tx(w[0].0), self.tx(w[0].1));
            let (mut xe, mut ye) = (self.tx(w[1].0), self.tx(w[1].1));
            if !(x.abs() < lim && y.abs() < lim && xe.abs() < lim && ye.abs() < lim) {
                return;
            }
            if x > xe {
                std::mem::swap(&mut x, &mut xe);
                std::mem::swap(&mut y, &mut ye);
            }
            if x.floor() == xe.floor() || (x - xe).abs() < 0.01 {
                if ye < y {
                    std::mem::swap(&mut y, &mut ye);
                }
                let ix = xe.floor() as i32;
                for iy in (y.floor() as i32)..=(ye.floor() as i32) {
                    self.cell(ix, iy);
                }
                continue;
            }
            if y.floor() == ye.floor() || (y - ye).abs() < 0.01 {
                let iy = y.floor() as i32;
                for ix in (x.floor() as i32)..=(xe.floor() as i32) {
                    self.cell(ix, iy);
                }
                continue;
            }
            let (xs, ys) = (x.floor() as i32, y.floor() as i32);
            x -= f64::from(xs);
            y -= f64::from(ys);
            xe -= f64::from(xs);
            ye -= f64::from(ys);
            let slope = (ye - y) / (xe - x);
            while x < xe {
                let (ix, iy) = (x.floor() as i32, y.floor() as i32);
                self.cell(ix + xs, iy + ys);
                let mut step_x = (x + 1.0).floor() - x;
                let mut step_y = step_x * slope;
                if (y + step_y).floor() as i32 == iy {
                    x += step_x;
                    y += step_y;
                } else if slope < 0.0 {
                    step_y = f64::from(iy) - y;
                    if step_y > -0.000_000_001 {
                        step_y = -0.000_000_001;
                    }
                    step_x = step_y / slope;
                    x += step_x;
                    y += step_y;
                } else {
                    step_y = f64::from(iy + 1) - y;
                    if step_y < 0.000_000_001 {
                        step_y = 0.000_000_001;
                    }
                    step_x = step_y / slope;
                    x += step_x;
                    y += step_y;
                }
            }
        }
    }

    /// `AddPolygonToIndex` over all rings of a shape: ring outlines plus a
    /// scanline fill (even-odd, which for valid Esri polygons is the union of
    /// their parts).
    fn polygon(&mut self, rings: &[Vec<(f64, f64)>]) {
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        for r in rings {
            self.line(r);
            for p in r {
                ymin = ymin.min(p.1);
                ymax = ymax.max(p.1);
            }
        }
        if !ymin.is_finite() {
            return;
        }
        let (miny, maxy) = (self.tx(ymin).floor() as i32, self.tx(ymax).floor() as i32);
        let mut xs = Vec::new();
        for iy in miny..=maxy {
            let dy = f64::from(iy) + 0.5;
            xs.clear();
            for r in rings {
                for w in r.windows(2) {
                    let (mut y1, mut y2) = (self.tx(w[0].1), self.tx(w[1].1));
                    if (y1 < dy && y2 < dy) || (y1 > dy && y2 > dy) {
                        continue;
                    }
                    let (x1, x2);
                    if y1 < y2 {
                        x1 = self.tx(w[0].0);
                        x2 = self.tx(w[1].0);
                    } else if y1 > y2 {
                        std::mem::swap(&mut y1, &mut y2);
                        x2 = self.tx(w[0].0);
                        x1 = self.tx(w[1].0);
                    } else {
                        let a = self.tx(w[0].0.min(w[1].0)).floor() as i32;
                        let b = self.tx(w[0].0.max(w[1].0)).floor() as i32;
                        for ix in a..=b {
                            self.cell(ix, iy);
                        }
                        continue;
                    }
                    if dy < y2 && dy >= y1 {
                        xs.push((dy - y1) * (x2 - x1) / (y2 - y1) + x1);
                    }
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.as_chunks::<2>().0 {
                for ix in (pair[0].floor() as i32)..=(pair[1].floor() as i32) {
                    self.cell(ix, iy);
                }
            }
        }
    }
}

/// `.spx` content for `shapes` ((OID, shape as stored, i.e. quantized)) on a
/// grid of `grid` units (0 = empty index).
pub(crate) fn spx(grid: f64, shapes: &[(i64, Shape)]) -> Vec<u8> {
    let mut values: Vec<(i64, i64)> = Vec::new();
    if grid > 0.0 {
        let mut b = Burner { grid, shift: f64::from(1u32 << 29), out: Vec::new() };
        for (oid, s) in shapes {
            b.out.clear();
            let s = s.densify(4.0); // GDAL's getLinearGeometry() default step
            let parts: Vec<Vec<(f64, f64)>> = s.parts.iter().map(|p| p.iter().map(|c| (c.x, c.y)).collect()).collect();
            match s.shape_type {
                ShapeType::Point | ShapeType::Multipoint => {
                    for &(x, y) in parts.iter().flatten() {
                        b.point(x, y);
                    }
                }
                ShapeType::Polyline => {
                    for p in &parts {
                        if p.len() >= 2 {
                            b.line(p);
                        }
                    }
                }
                ShapeType::Polygon => b.polygon(&parts),
                ShapeType::MultiPatch => {}
            }
            b.out.sort_unstable();
            b.out.dedup();
            values.extend(b.out.iter().map(|&v| (v, *oid)));
        }
    }
    values.sort_unstable();
    write_index(&values)
}

fn write_value(page: &mut Vec<u8>, v: i64) {
    page.extend_from_slice(&(v as u64).to_le_bytes());
}

/// Port of GDAL's `WriteIndex` for 8-byte values (depth 1 to 4).
fn write_index(values: &[(i64, i64)]) -> Vec<u8> {
    let n = values.len();
    let pp = PER_PAGE;
    let mut out = Vec::new();
    let finish_page = |out: &mut Vec<u8>, page: &mut Vec<u8>| {
        page.resize(PAGE, 0);
        out.extend_from_slice(page);
        page.clear();
    };
    let root = |out: &mut Vec<u8>, n_children: usize, mult: usize| {
        let mut page = Vec::with_capacity(PAGE);
        put_u32(&mut page, 0);
        put_u32(&mut page, if n_children == 1 { 1 } else { n_children as u32 - 1 });
        for i in 0..n_children {
            put_u32(&mut page, 2 + i as u32);
        }
        page.resize(OFFSET_FIRST_VAL, 0);
        if n_children == 1 {
            write_value(&mut page, values[n - 1].0);
        } else {
            for i in 0..n_children - 1 {
                write_value(&mut page, values[(i + 1) * mult - 1].0);
            }
        }
        finish_page(out, &mut page);
    };
    let leaves = |out: &mut Vec<u8>, base: usize, n_pages: usize| {
        let mut page = Vec::with_capacity(PAGE);
        for i in 0..n_pages {
            let mut cnt = pp;
            if i + 1 < n_pages {
                put_u32(&mut page, (base + i + 1) as u32);
            } else {
                put_u32(&mut page, 0);
                cnt = n - i * pp;
            }
            put_u32(&mut page, cnt as u32);
            put_u32(&mut page, 0);
            for j in 0..cnt {
                put_u32(&mut page, values[i * pp + j].1 as u32);
            }
            page.resize(OFFSET_FIRST_VAL, 0);
            for j in 0..cnt {
                write_value(&mut page, values[i * pp + j].0);
            }
            finish_page(out, &mut page);
        }
    };
    let intermediate = |out: &mut Vec<u8>, base: usize, n_this: usize, n_next: usize, mult: usize| {
        let mut page = Vec::with_capacity(PAGE);
        for i in 0..n_this {
            let mut cnt = pp;
            if i + 1 < n_this {
                put_u32(&mut page, (base + i + 1) as u32);
            } else {
                put_u32(&mut page, 0);
                cnt = n_next - i * pp - 1;
            }
            put_u32(&mut page, cnt as u32);
            for j in 0..=cnt {
                put_u32(&mut page, (base + n_this + i * pp + j) as u32);
            }
            page.resize(OFFSET_FIRST_VAL, 0);
            for j in 0..cnt {
                write_value(&mut page, values[(i * pp + j + 1) * mult - 1].0);
            }
            finish_page(out, &mut page);
        }
    };
    let n_leaf = n.div_ceil(pp);
    let up = |k: usize| if k == 1 { 1 } else { (k - 1).div_ceil(pp) };
    let depth: u32;
    if n <= pp {
        depth = 1;
        let mut page = Vec::with_capacity(PAGE);
        put_u32(&mut page, 0);
        put_u32(&mut page, n as u32);
        put_u32(&mut page, 0);
        for v in values {
            put_u32(&mut page, v.1 as u32);
        }
        page.resize(OFFSET_FIRST_VAL, 0);
        for v in values {
            write_value(&mut page, v.0);
        }
        finish_page(&mut out, &mut page);
    } else if n <= (pp + 1) * pp {
        depth = 2;
        root(&mut out, n_leaf, pp);
        leaves(&mut out, 2, n_leaf);
    } else if n <= ((pp + 1) * pp + 1) * pp {
        depth = 3;
        let n2 = up(n_leaf);
        root(&mut out, n2, pp * pp);
        intermediate(&mut out, 2, n2, n_leaf, pp);
        leaves(&mut out, 2 + n2, n_leaf);
    } else {
        depth = 4;
        let n3 = up(n_leaf);
        let n2 = up(n3);
        root(&mut out, n2, pp * pp * pp);
        intermediate(&mut out, 2, n2, n3, pp * pp);
        intermediate(&mut out, 2 + n2, n3, n_leaf, pp);
        leaves(&mut out, 2 + n2 + n3, n_leaf);
    }
    out.push(8);
    out.push(0x40);
    put_u32(&mut out, 1);
    put_u32(&mut out, depth);
    put_u32(&mut out, n as u32);
    put_u32(&mut out, 0);
    put_u32(&mut out, 1);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filegdb::Coord;

    #[test]
    fn empty_index_matches_arcgis() {
        // ArcGIS Pro 3.6: empty feature class -> one zeroed page + trailer.
        let b = spx(0.0, &[]);
        assert_eq!(b.len(), 4118);
        assert!(b[..4096].iter().all(|&x| x == 0));
        assert_eq!(&b[4096..], &[8, 0x40, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn polygon_cells_match_arcgis() {
        // Reference Analysis_Boundary (ArcGIS, grid 26000): cells x 536871033..34,
        // y 536871442..43 for a polygon spanning those cells.
        let g = 26000.0;
        let x0 = (536_871_033.0 - 536_870_912.0) * g + 100.0;
        let y0 = (536_871_442.0 - 536_870_912.0) * g + 100.0;
        let ring = vec![Coord::xy(x0, y0), Coord::xy(x0, y0 + g), Coord::xy(x0 + g, y0 + g), Coord::xy(x0 + g, y0), Coord::xy(x0, y0)];
        let b = spx(g, &[(1, Shape::new(ShapeType::Polygon, vec![ring]))]);
        let cnt = u32::from_le_bytes(b[4..8].try_into().unwrap());
        assert_eq!(cnt, 4);
        let vals: Vec<(u64, u64)> = (0..4)
            .map(|i| {
                let o = OFFSET_FIRST_VAL + 8 * i;
                let v = u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
                ((v >> 31) & 0x7FFF_FFFF, v & 0x7FFF_FFFF)
            })
            .collect();
        assert_eq!(
            vals,
            vec![(536_871_033, 536_871_442), (536_871_033, 536_871_443), (536_871_034, 536_871_442), (536_871_034, 536_871_443)]
        );
    }

    #[test]
    fn deep_index_page_counts() {
        // depth 2 and 3 page layout sizes
        let v2: Vec<(i64, i64)> = (0..1000).map(|i| (i, i)).collect();
        assert_eq!(write_index(&v2).len(), 4096 * (1 + 3) + 22);
        let n3 = (PER_PAGE + 1) * PER_PAGE + 5;
        let v3: Vec<(i64, i64)> = (0..n3 as i64).map(|i| (i, i)).collect();
        let leaves = n3.div_ceil(PER_PAGE);
        let lvl2 = (leaves - 1).div_ceil(PER_PAGE);
        let b = write_index(&v3);
        assert_eq!(b.len(), 4096 * (1 + lvl2 + leaves) + 22);
        assert_eq!(u32::from_le_bytes(b[b.len() - 16..b.len() - 12].try_into().unwrap()), 3);
    }
}
