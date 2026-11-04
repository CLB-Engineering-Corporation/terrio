// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable.cpp

//! Decoding (and encoding) of the compressed File Geodatabase shape buffer.
//!
//! Ported from GDAL's `FileGDBOGRGeometryConverterImpl::GetAsGeometry`
//! (`filegdbtable.cpp`) and `FileGDBTable::EncodeGeometry`
//! (`filegdbtable_write.cpp`). Coordinates are reconstructed with exactly the
//! same arithmetic as GDAL (`integer / scale + origin`), so values are
//! bit-identical to what GDAL (and ArcGIS) report.

use crate::varint::{Cursor, write_varint, write_varuint};
use crate::{Coord, CurveSegment, Error, GeometryFieldInfo, Result, Shape, ShapeType};

const SEGMENT_ARC: u32 = 1;
const SEGMENT_BEZIER: u32 = 4;
const SEGMENT_ELLIPSE: u32 = 5;
const ARC_EMPTY: u32 = 0x1;
const ARC_CCW: u32 = 0x8;
const ARC_LINE: u32 = 0x20;
const ARC_POINT: u32 = 0x40;
const ARC_IP: u32 = 0x80;

pub(crate) const EXT_SHAPE_Z_FLAG: u32 = 0x8000_0000;
pub(crate) const EXT_SHAPE_M_FLAG: u32 = 0x4000_0000;
pub(crate) const EXT_SHAPE_CURVE_FLAG: u32 = 0x2000_0000;

const SHPT_NULL: u32 = 0;
const SHPT_POINT: u32 = 1;
const SHPT_POINTM: u32 = 21;
const SHPT_POINTZM: u32 = 11;
const SHPT_POINTZ: u32 = 9;
const SHPT_MULTIPOINT: u32 = 8;
const SHPT_MULTIPOINTM: u32 = 28;
const SHPT_MULTIPOINTZM: u32 = 18;
const SHPT_MULTIPOINTZ: u32 = 20;
const SHPT_ARC: u32 = 3;
const SHPT_ARCM: u32 = 23;
const SHPT_ARCZM: u32 = 13;
const SHPT_ARCZ: u32 = 10;
const SHPT_POLYGON: u32 = 5;
const SHPT_POLYGONM: u32 = 25;
const SHPT_POLYGONZM: u32 = 15;
const SHPT_POLYGONZ: u32 = 19;
const SHPT_MULTIPATCHM: u32 = 31;
const SHPT_MULTIPATCH: u32 = 32;
const SHPT_GENERALPOLYLINE: u32 = 50;
const SHPT_GENERALPOLYGON: u32 = 51;
const SHPT_GENERALPOINT: u32 = 52;
const SHPT_GENERALMULTIPOINT: u32 = 53;
const SHPT_GENERALMULTIPATCH: u32 = 54;

/// GDAL's `SanitizeScale`: a zero/negative scale is replaced by a sane value.
fn sanitize_scale(v: f64) -> f64 {
    if v == 0.0 { f64::MIN_POSITIVE } else { v }
}

struct Decoder<'a, 'g> {
    cur: Cursor<'a>,
    g: &'g GeometryFieldInfo,
}

impl Decoder<'_, '_> {
    fn xy(&mut self, n: usize, dx: &mut i64, dy: &mut i64, out: &mut Vec<Coord>) -> Result<()> {
        for _ in 0..n {
            *dx = dx.wrapping_add(self.cur.varint()?);
            *dy = dy.wrapping_add(self.cur.varint()?);
            out.push(Coord {
                x: *dx as f64 / self.g.xy_scale + self.g.x_origin,
                y: *dy as f64 / self.g.xy_scale + self.g.y_origin,
                z: None,
                m: None,
            });
        }
        Ok(())
    }

    fn z(&mut self, coords: &mut [Coord], dz: &mut i64) -> Result<()> {
        let scale = sanitize_scale(self.g.z_scale);
        for c in coords {
            *dz = dz.wrapping_add(self.cur.varint()?);
            c.z = Some(*dz as f64 / scale + self.g.z_origin);
        }
        Ok(())
    }

    fn m(&mut self, coords: &mut [Coord], dm: &mut i64) -> Result<()> {
        let scale = sanitize_scale(self.g.m_scale);
        for c in coords {
            *dm = dm.wrapping_add(self.cur.varint()?);
            c.m = Some(*dm as f64 / scale + self.g.m_origin);
        }
        Ok(())
    }

    /// Reads point/part counts and the per-part point counts (GDAL `ReadPartDefs`).
    fn part_defs(&mut self, has_curve_desc: bool, multipatch: bool) -> Result<(usize, Vec<usize>, u32)> {
        let n_points = self.cur.varuint32()? as usize;
        if n_points == 0 {
            return Ok((0, Vec::new(), 0));
        }
        if n_points > self.cur.remaining() {
            return Err(Error::Format("point count exceeds geometry size".into()));
        }
        if multipatch {
            self.cur.varuint()?;
        }
        let n_parts = self.cur.varuint32()? as usize;
        if n_parts > self.cur.remaining() {
            return Err(Error::Format("part count exceeds geometry size".into()));
        }
        let n_curves = if has_curve_desc { self.cur.varuint32()? } else { 0 };
        if n_parts == 0 {
            return Ok((n_points, Vec::new(), n_curves));
        }
        // Bounding box: xmin, ymin, dx, dy.
        for _ in 0..4 {
            self.cur.varuint()?;
        }
        let mut counts = Vec::with_capacity(n_parts);
        let mut sum = 0usize;
        for _ in 0..n_parts - 1 {
            let c = self.cur.varuint32()? as usize;
            sum += c;
            counts.push(c);
        }
        if sum > n_points {
            return Err(Error::Format("part point counts exceed point count".into()));
        }
        counts.push(n_points - sum);
        Ok((n_points, counts, n_curves))
    }

    fn multipart(&mut self, counts: &[usize], has_z: bool, has_m: bool) -> Result<Vec<Vec<Coord>>> {
        let (mut dx, mut dy) = (0i64, 0i64);
        let mut parts = Vec::with_capacity(counts.len());
        for &n in counts {
            let mut p = Vec::with_capacity(n);
            self.xy(n, &mut dx, &mut dy, &mut p)?;
            parts.push(p);
        }
        if has_z {
            let mut dz = 0i64;
            for p in &mut parts {
                self.z(p, &mut dz)?;
            }
        }
        if has_m {
            let mut dm = 0i64;
            for p in &mut parts {
                // Absence of M is sometimes marked by a single byte (66): only
                // parse when enough bytes remain, as GDAL does.
                if self.cur.remaining() < p.len() {
                    for q in &mut parts {
                        for c in q.iter_mut() {
                            c.m = None;
                        }
                    }
                    break;
                }
                self.m(p, &mut dm)?;
            }
        }
        Ok(parts)
    }
}

/// Decodes a geometry blob. Returns `None` for a null shape.
pub(crate) fn decode(blob: &[u8], g: &GeometryFieldInfo) -> Result<Option<Shape>> {
    let mut d = Decoder { cur: Cursor::new(blob), g };
    let geom_type = d.cur.varuint32()?;
    let mut has_z = geom_type & EXT_SHAPE_Z_FLAG != 0;
    let mut has_m = geom_type & EXT_SHAPE_M_FLAG != 0;
    let curve_flag = geom_type & EXT_SHAPE_CURVE_FLAG != 0;
    let base = geom_type & 0xff;
    match base {
        SHPT_NULL => Ok(None),
        SHPT_POINTZ | SHPT_POINTZM | SHPT_POINT | SHPT_POINTM | SHPT_GENERALPOINT => {
            if matches!(base, SHPT_POINTZ | SHPT_POINTZM) {
                has_z = true;
            }
            if matches!(base, SHPT_POINTM | SHPT_POINTZM) {
                has_m = true;
            }
            let x = d.cur.varuint()?;
            let y = d.cur.varuint()?;
            let decode1 = |v: u64, scale: f64, origin: f64| {
                if v == 0 { f64::NAN } else { (v - 1) as f64 / scale + origin }
            };
            let mut c = Coord { x: decode1(x, g.xy_scale, g.x_origin), y: decode1(y, g.xy_scale, g.y_origin), z: None, m: None };
            if has_z {
                let z = d.cur.varuint()?;
                c.z = Some(decode1(z, sanitize_scale(g.z_scale), g.z_origin));
            }
            if has_m && d.cur.remaining() > 0 {
                let m = d.cur.varuint()?;
                c.m = Some(decode1(m, sanitize_scale(g.m_scale), g.m_origin));
            }
            let parts = if x == 0 { Vec::new() } else { vec![vec![c]] };
            Ok(Some(Shape { shape_type: ShapeType::Point, parts, has_curves: false, curves: Vec::new() }))
        }
        SHPT_MULTIPOINTZM | SHPT_MULTIPOINTZ | SHPT_MULTIPOINT | SHPT_MULTIPOINTM | SHPT_GENERALMULTIPOINT => {
            if matches!(base, SHPT_MULTIPOINTZ | SHPT_MULTIPOINTZM) {
                has_z = true;
            }
            if matches!(base, SHPT_MULTIPOINTM | SHPT_MULTIPOINTZM) {
                has_m = true;
            }
            let n = d.cur.varuint32()? as usize;
            if n == 0 {
                return Ok(Some(Shape { shape_type: ShapeType::Multipoint, parts: Vec::new(), has_curves: false, curves: Vec::new() }));
            }
            for _ in 0..4 {
                d.cur.varuint()?;
            }
            let parts = d.multipart(&[n], has_z, has_m)?;
            let parts = parts.into_iter().flatten().map(|c| vec![c]).collect();
            Ok(Some(Shape { shape_type: ShapeType::Multipoint, parts, has_curves: false, curves: Vec::new() }))
        }
        SHPT_ARCZ | SHPT_ARCZM | SHPT_ARC | SHPT_ARCM | SHPT_GENERALPOLYLINE | SHPT_POLYGONZ | SHPT_POLYGONZM | SHPT_POLYGON
        | SHPT_POLYGONM | SHPT_GENERALPOLYGON => {
            let is_poly = matches!(base, SHPT_POLYGONZ | SHPT_POLYGONZM | SHPT_POLYGON | SHPT_POLYGONM | SHPT_GENERALPOLYGON);
            if matches!(base, SHPT_ARCZ | SHPT_ARCZM | SHPT_POLYGONZ | SHPT_POLYGONZM) {
                has_z = true;
            }
            if matches!(base, SHPT_ARCM | SHPT_ARCZM | SHPT_POLYGONM | SHPT_POLYGONZM) {
                has_m = true;
            }
            let shape_type = if is_poly { ShapeType::Polygon } else { ShapeType::Polyline };
            let (n_points, counts, n_curves) = d.part_defs(curve_flag, false)?;
            if n_points == 0 || counts.is_empty() {
                return Ok(Some(Shape { shape_type, parts: Vec::new(), has_curves: false, curves: Vec::new() }));
            }
            if n_curves == 0 {
                let parts = d.multipart(&counts, has_z, has_m)?;
                return Ok(Some(Shape { shape_type, parts, has_curves: false, curves: Vec::new() }));
            }
            // Curved geometry (GDAL `CreateCurveGeometry`): XY, Z, then M (a
            // single byte 66 marks absent M), then the segment descriptors.
            let mut parts = d.multipart(&counts, has_z, false)?;
            if has_m {
                if d.cur.peek() == Some(66) {
                    d.cur.u8()?;
                } else {
                    let mut dm = 0i64;
                    for p in &mut parts {
                        d.m(p, &mut dm)?;
                    }
                }
            }
            let mut curves = Vec::with_capacity(n_curves as usize);
            for _ in 0..n_curves {
                let start = d.cur.varuint32()? as usize;
                let kind = d.cur.varuint32()?;
                match kind {
                    SEGMENT_ARC => {
                        let v1 = d.cur.f64()?;
                        let v2 = d.cur.f64()?;
                        let bits = d.cur.u32()?;
                        if bits & ARC_IP != 0 && bits & ARC_LINE == 0 {
                            curves.push(CurveSegment::ArcInteriorPoint { start_index: start, x: v1, y: v2, ccw: bits & ARC_CCW != 0 });
                        } else if bits & (ARC_EMPTY | ARC_LINE | ARC_POINT) == 0 {
                            curves.push(CurveSegment::ArcCenterPoint {
                                start_index: start,
                                center_x: v1,
                                center_y: v2,
                                ccw: bits & ARC_CCW != 0,
                            });
                        }
                        // Arcs flagged empty/line/point degenerate to straight segments.
                    }
                    SEGMENT_BEZIER => {
                        let c = [d.cur.f64()?, d.cur.f64()?, d.cur.f64()?, d.cur.f64()?];
                        curves.push(CurveSegment::Bezier { start_index: start, x1: c[0], y1: c[1], x2: c[2], y2: c[3] });
                    }
                    SEGMENT_ELLIPSE => {
                        let v = [d.cur.f64()?, d.cur.f64()?, d.cur.f64()?, d.cur.f64()?, d.cur.f64()?];
                        let bits = d.cur.u32()?;
                        curves.push(CurveSegment::Ellipse { start_index: start, params: v, bits });
                    }
                    other => return Err(Error::Unsupported(format!("curve segment type {other}"))),
                }
            }
            Ok(Some(Shape { shape_type, parts, has_curves: true, curves }))
        }
        SHPT_MULTIPATCHM | SHPT_MULTIPATCH | SHPT_GENERALMULTIPATCH => {
            if base != SHPT_GENERALMULTIPATCH {
                has_z = true;
            }
            let (n_points, counts, _) = d.part_defs(false, true)?;
            if n_points == 0 || counts.is_empty() {
                return Ok(Some(Shape { shape_type: ShapeType::MultiPatch, parts: Vec::new(), has_curves: false, curves: Vec::new() }));
            }
            for _ in 0..counts.len() {
                d.cur.varuint()?; // part types
            }
            let all = d.multipart(&[n_points], has_z, false)?.pop().unwrap_or_default();
            let mut parts = Vec::with_capacity(counts.len());
            let mut it = all.into_iter();
            for &n in &counts {
                parts.push(it.by_ref().take(n).collect());
            }
            Ok(Some(Shape { shape_type: ShapeType::MultiPatch, parts, has_curves: false, curves: Vec::new() }))
        }
        other => Err(Error::Unsupported(format!("geometry type {other}"))),
    }
}

/// Returns the envelope (xmin, ymin, xmax, ymax) of a geometry blob without
/// decoding vertices (GDAL `GetFeatureExtent`). `None` for null/empty shapes.
pub(crate) fn blob_extent(blob: &[u8], g: &GeometryFieldInfo) -> Result<Option<[f64; 4]>> {
    let mut c = Cursor::new(blob);
    let geom_type = c.varuint32()?;
    let base = geom_type & 0xff;
    let to_skip = match base {
        SHPT_NULL => return Ok(None),
        SHPT_POINTZ | SHPT_POINTZM | SHPT_POINT | SHPT_POINTM | SHPT_GENERALPOINT => {
            let x = c.varuint()?;
            let y = c.varuint()?;
            if x == 0 {
                return Ok(None);
            }
            let px = (x - 1) as f64 / g.xy_scale + g.x_origin;
            let py = (y.wrapping_sub(1)) as f64 / g.xy_scale + g.y_origin;
            return Ok(Some([px, py, px, py]));
        }
        SHPT_MULTIPOINTZM | SHPT_MULTIPOINTZ | SHPT_MULTIPOINT | SHPT_MULTIPOINTM | SHPT_GENERALMULTIPOINT => 0,
        SHPT_ARC | SHPT_ARCZ | SHPT_ARCZM | SHPT_ARCM | SHPT_POLYGON | SHPT_POLYGONZ | SHPT_POLYGONZM | SHPT_POLYGONM => 1,
        SHPT_GENERALPOLYLINE | SHPT_GENERALPOLYGON => 1 + usize::from(geom_type & EXT_SHAPE_CURVE_FLAG != 0),
        SHPT_GENERALMULTIPATCH | SHPT_MULTIPATCHM | SHPT_MULTIPATCH => 2,
        _ => return Ok(None),
    };
    let n_points = c.varuint()?;
    if n_points == 0 {
        return Ok(None);
    }
    for _ in 0..to_skip {
        c.varuint()?;
    }
    let xmin = c.varuint()?;
    let ymin = c.varuint()?;
    let dx = c.varuint()?;
    let dy = c.varuint()?;
    Ok(Some([
        xmin as f64 / g.xy_scale + g.x_origin,
        ymin as f64 / g.xy_scale + g.y_origin,
        xmin.wrapping_add(dx) as f64 / g.xy_scale + g.x_origin,
        ymin.wrapping_add(dy) as f64 / g.xy_scale + g.y_origin,
    ]))
}

/// Quantizes a coordinate to the integer grid of the geometry field
/// (GDAL: `std::round((x - origin) * scale)`).
fn quantize(v: f64, origin: f64, scale: f64) -> Result<i64> {
    let q = ((v - origin) * scale).round();
    if !q.is_finite() || q < 0.0 || q > 9.0e18 {
        return Err(Error::InvalidArgument(format!(
            "coordinate {v} is outside the domain of the spatial reference (origin {origin}, scale {scale})"
        )));
    }
    Ok(q as i64)
}

/// Encodes a shape into the compressed shape buffer. Z/M values are written
/// only when `g.has_z` / `g.has_m`. Curves are not supported for writing.
pub(crate) fn encode(shape: &Shape, g: &GeometryFieldInfo) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let zflag = if g.has_z { EXT_SHAPE_Z_FLAG } else { 0 };
    let mflag = if g.has_m { EXT_SHAPE_M_FLAG } else { 0 };
    match shape.shape_type {
        ShapeType::Point => {
            let Some(c) = shape.parts.first().and_then(|p| p.first()) else {
                // Empty point.
                write_varuint(&mut out, u64::from(SHPT_GENERALPOINT | zflag | mflag));
                write_varuint(&mut out, 0);
                write_varuint(&mut out, 0);
                return Ok(out);
            };
            let base = match (g.has_z, g.has_m) {
                (false, false) => SHPT_POINT,
                (true, false) => SHPT_POINTZ,
                (false, true) => SHPT_POINTM,
                (true, true) => SHPT_POINTZM,
            };
            write_varuint(&mut out, u64::from(base));
            write_varuint(&mut out, quantize(c.x, g.x_origin, g.xy_scale)? as u64 + 1);
            write_varuint(&mut out, quantize(c.y, g.y_origin, g.xy_scale)? as u64 + 1);
            if g.has_z {
                write_varuint(&mut out, quantize(c.z.unwrap_or(0.0), g.z_origin, g.z_scale)? as u64 + 1);
            }
            if g.has_m {
                match c.m {
                    Some(m) => write_varuint(&mut out, quantize(m, g.m_origin, g.m_scale)? as u64 + 1),
                    None => write_varuint(&mut out, 0),
                }
            }
            Ok(out)
        }
        ShapeType::Polyline | ShapeType::Polygon | ShapeType::Multipoint => {
            let is_mp = shape.shape_type == ShapeType::Multipoint;
            let base = match (shape.shape_type, g.has_z, g.has_m) {
                (ShapeType::Polyline, false, false) => SHPT_ARC,
                (ShapeType::Polyline, true, false) => SHPT_ARCZ,
                (ShapeType::Polyline, false, true) => SHPT_ARCM,
                (ShapeType::Polyline, true, true) => SHPT_ARCZM,
                (ShapeType::Polygon, false, false) => SHPT_POLYGON,
                (ShapeType::Polygon, true, false) => SHPT_POLYGONZ,
                (ShapeType::Polygon, false, true) => SHPT_POLYGONM,
                (ShapeType::Polygon, true, true) => SHPT_POLYGONZM,
                (_, false, false) => SHPT_MULTIPOINT,
                (_, true, false) => SHPT_MULTIPOINTZ,
                (_, false, true) => SHPT_MULTIPOINTM,
                (_, true, true) => SHPT_MULTIPOINTZM,
            };
            write_varuint(&mut out, u64::from(base));
            let parts: Vec<&Vec<Coord>> = shape.parts.iter().filter(|p| !p.is_empty()).collect();
            let n_points: usize = parts.iter().map(|p| p.len()).sum();
            write_varuint(&mut out, n_points as u64);
            if n_points == 0 {
                if !is_mp {
                    write_varuint(&mut out, 0);
                }
                return Ok(out);
            }
            if !is_mp {
                write_varuint(&mut out, parts.len() as u64);
            }
            // Quantize first, then compute the bbox on the integer grid.
            let mut q: Vec<(i64, i64)> = Vec::with_capacity(n_points);
            for p in &parts {
                for c in p.iter() {
                    q.push((quantize(c.x, g.x_origin, g.xy_scale)?, quantize(c.y, g.y_origin, g.xy_scale)?));
                }
            }
            let xmin = q.iter().map(|v| v.0).min().unwrap_or(0);
            let ymin = q.iter().map(|v| v.1).min().unwrap_or(0);
            let xmax = q.iter().map(|v| v.0).max().unwrap_or(0);
            let ymax = q.iter().map(|v| v.1).max().unwrap_or(0);
            write_varuint(&mut out, xmin as u64);
            write_varuint(&mut out, ymin as u64);
            write_varuint(&mut out, (xmax - xmin) as u64);
            write_varuint(&mut out, (ymax - ymin) as u64);
            if !is_mp {
                for p in &parts[..parts.len() - 1] {
                    write_varuint(&mut out, p.len() as u64);
                }
            }
            let (mut px, mut py) = (0i64, 0i64);
            for (x, y) in &q {
                write_varint(&mut out, x - px);
                write_varint(&mut out, y - py);
                px = *x;
                py = *y;
            }
            if g.has_z {
                let mut pz = 0i64;
                for c in parts.iter().flat_map(|p| p.iter()) {
                    let z = quantize(c.z.unwrap_or(0.0), g.z_origin, g.z_scale)?;
                    write_varint(&mut out, z - pz);
                    pz = z;
                }
            }
            if g.has_m {
                let mut pm = 0i64;
                for c in parts.iter().flat_map(|p| p.iter()) {
                    let m = quantize(c.m.unwrap_or(0.0), g.m_origin, g.m_scale)?;
                    write_varint(&mut out, m - pm);
                    pm = m;
                }
            }
            Ok(out)
        }
        ShapeType::MultiPatch => Err(Error::Unsupported("writing multipatch geometries".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gfi(shape_type: ShapeType) -> GeometryFieldInfo {
        GeometryFieldInfo {
            shape_type,
            has_z: false,
            has_m: false,
            wkt: String::new(),
            x_origin: -1000.0,
            y_origin: -2000.0,
            xy_scale: 1000.0,
            z_origin: 0.0,
            z_scale: 1.0,
            m_origin: 0.0,
            m_scale: 1.0,
            xy_tolerance: 0.001,
            z_tolerance: 0.001,
            m_tolerance: 0.001,
            extent: [0.0; 4],
        }
    }

    fn c(x: f64, y: f64) -> Coord {
        Coord { x, y, z: None, m: None }
    }

    #[test]
    fn polygon_roundtrip() {
        let g = gfi(ShapeType::Polygon);
        let s = Shape {
            shape_type: ShapeType::Polygon,
            parts: vec![
                vec![c(0.0, 0.0), c(0.0, 10.0), c(10.0, 10.0), c(10.0, 0.0), c(0.0, 0.0)],
                vec![c(2.0, 2.0), c(4.0, 2.0), c(4.0, 4.0), c(2.0, 4.0), c(2.0, 2.0)],
            ],
            has_curves: false,
            curves: Vec::new(),
        };
        let blob = encode(&s, &g).unwrap();
        assert_eq!(decode(&blob, &g).unwrap().unwrap(), s);
        assert_eq!(blob_extent(&blob, &g).unwrap().unwrap(), [0.0, 0.0, 10.0, 10.0]);
    }

    #[test]
    fn point_roundtrip() {
        let g = gfi(ShapeType::Point);
        let s = Shape { shape_type: ShapeType::Point, parts: vec![vec![c(1.5, -3.25)]], has_curves: false, curves: Vec::new() };
        let blob = encode(&s, &g).unwrap();
        assert_eq!(decode(&blob, &g).unwrap().unwrap(), s);
    }

    #[test]
    fn polyline_quantizes() {
        let g = gfi(ShapeType::Polyline);
        let s = Shape {
            shape_type: ShapeType::Polyline,
            parts: vec![vec![c(0.00049, 1.0), c(5.0, 1.0004)]],
            has_curves: false,
            curves: Vec::new(),
        };
        let d = decode(&encode(&s, &g).unwrap(), &g).unwrap().unwrap();
        assert_eq!(d.parts[0][0].x, 0.0);
        assert_eq!(d.parts[0][1].y, 1.0);
    }
}
