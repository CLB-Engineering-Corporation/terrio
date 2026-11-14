// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2022, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable_write.cpp, filegdbindex_write.cpp, ogropenfilegdblayer_write.cpp

//! Creation of new File Geodatabases.
//!
//! Ported from GDAL's OpenFileGDB writer (`filegdbtable_write.cpp`,
//! `filegdbtable_write_fields.cpp`, `ogropenfilegdbdatasource_write.cpp`,
//! `ogropenfilegdblayer_write.cpp`). The system tables are those GDAL creates;
//! the XML item definitions follow what ArcGIS Pro 3.x writes (namespace
//! 10.8), read from geodatabases created by ArcGIS.
//!
//! Tables are buffered in memory and written by [`GdbWriter::finish`].
//! Every user table gets a `.gdbindexes` registering `FDO_OBJECTID` and,
//! for feature classes, `FDO_Shape` with its `.spx` spatial index (see
//! `spatial_index.rs`), as ArcGIS writes them. No attribute indexes (`.atx`).

use std::collections::BTreeSet;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};

use crate::filegdb::geometry::{self, EXT_SHAPE_CURVE_FLAG, EXT_SHAPE_M_FLAG, EXT_SHAPE_Z_FLAG};
use crate::filegdb::spatial_index;
use crate::filegdb::varint::{to_utf16, write_varint, write_varuint};
use crate::filegdb::{CurveSegment, Error, FieldType, GeometryFieldInfo, Result, Shape, ShapeType, Value};

const FOLDER_TYPE: &str = "{f3783e6f-65ca-4514-8315-ce3985dad3b1}";
const WORKSPACE_TYPE: &str = "{c673fe0f-7280-404f-8532-20755dd8fc06}";
const FEATURE_CLASS_TYPE: &str = "{70737809-852c-4a03-9e22-2cecea5b9bfa}";
const TABLE_TYPE: &str = "{cd06bc3b-789d-4c51-aafa-a467912b8965}";
const CODED_DOMAIN_TYPE: &str = "{8c368b12-a12e-4c7e-9638-c9c64e69e98f}";
const DATASET_IN_FOLDER: &str = "{dc78f1ab-34e4-43ac-ba47-1c4eabd0e7c7}";
const DOMAIN_IN_DATASET: &str = "{17e08adb-2b31-4dcd-8fdd-df529e88f843}";

/// ESRI WKT of EPSG:2278 as ArcGIS Pro writes it.
pub const ESRI_WKT_2278: &str = "PROJCS[\"NAD_1983_StatePlane_Texas_South_Central_FIPS_4204_Feet\",GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Lambert_Conformal_Conic\"],PARAMETER[\"False_Easting\",1968500.0],PARAMETER[\"False_Northing\",13123333.33333333],PARAMETER[\"Central_Meridian\",-99.0],PARAMETER[\"Standard_Parallel_1\",28.38333333333333],PARAMETER[\"Standard_Parallel_2\",30.28333333333333],PARAMETER[\"Latitude_Of_Origin\",27.83333333333333],UNIT[\"Foot_US\",0.3048006096012192],AUTHORITY[\"EPSG\",2278]]";

/// The horizon of EPSG:2278 in ArcGIS Pro (xmin, ymin, xmax, ymax; feet), the content of the
/// `.horizon` file ArcGIS writes for every EPSG:2278 feature class, whatever its XY grid.
pub const HORIZON_2278: [f64; 4] = [-126_725_700.0, -77_828_800.0, 130_662_700.0, 50_930_400.0];

const ESRI_WKT_WGS84: &str = "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]";

/// The geometry of a feature class to create.
#[derive(Debug, Clone)]
pub struct GeometryDef {
    /// Geometry type.
    pub shape_type: ShapeType,
    /// ESRI WKT of the coordinate system.
    pub wkt: String,
    /// The (possibly Esri) WKID, e.g. 102740 for EPSG:2278.
    pub wkid: Option<u32>,
    /// The latest (EPSG) WKID, e.g. 2278.
    pub latest_wkid: Option<u32>,
    /// X origin of the coordinate grid.
    pub x_origin: f64,
    /// Y origin of the coordinate grid.
    pub y_origin: f64,
    /// XY scale (1 / resolution).
    pub xy_scale: f64,
    /// XY tolerance.
    pub xy_tolerance: f64,
}

impl GeometryDef {
    /// EPSG:2278 (NAD83 / Texas South Central, US feet) with the coordinate
    /// grid ArcGIS Pro uses by default (origin -126725700, -77828800; scale
    /// 3048.0060960121928; tolerance 0.0032808333 ft).
    pub fn epsg_2278(shape_type: ShapeType) -> Self {
        Self {
            shape_type,
            wkt: ESRI_WKT_2278.to_string(),
            wkid: Some(102740),
            latest_wkid: Some(2278),
            x_origin: -126_725_700.0,
            y_origin: -77_828_800.0,
            xy_scale: 3_048.006_096_012_192_8,
            xy_tolerance: 0.003_280_833_333_333_333,
        }
    }

    /// The coordinate system's horizon, as ArcGIS Pro writes it in a feature class's
    /// `.horizon` file (known for EPSG:2278 only; other systems get none, and ArcGIS writes
    /// the file when it first opens the feature class).
    pub fn horizon(&self) -> Option<[f64; 4]> {
        (self.latest_wkid == Some(2278) || self.wkid == Some(102740)).then_some(HORIZON_2278)
    }

    fn info(&self) -> GeometryFieldInfo {
        GeometryFieldInfo {
            shape_type: self.shape_type,
            has_z: false,
            has_m: false,
            wkt: self.wkt.clone(),
            x_origin: self.x_origin,
            y_origin: self.y_origin,
            xy_scale: self.xy_scale,
            z_origin: -100_000.0,
            z_scale: 10_000.0,
            m_origin: -100_000.0,
            m_scale: 10_000.0,
            xy_tolerance: self.xy_tolerance,
            z_tolerance: 0.001,
            m_tolerance: 0.001,
            extent: [f64::NAN; 4],
        }
    }
}

/// An attribute field of a table to create.
#[derive(Debug, Clone)]
pub struct FieldDef {
    /// Name.
    pub name: String,
    /// Alias (may be empty).
    pub alias: String,
    /// Type: Int16, Int32, Int64, Float32, Float64, String or DateTime.
    pub field_type: FieldType,
    /// Whether nulls are allowed.
    pub nullable: bool,
    /// Maximum length for text fields.
    pub length: u32,
    /// Name of a coded-value domain added with [`GdbWriter::add_coded_domain`].
    pub domain: Option<String>,
}

impl FieldDef {
    /// A nullable field without alias or domain.
    pub fn new(name: impl Into<String>, field_type: FieldType) -> Self {
        Self { name: name.into(), alias: String::new(), field_type, nullable: true, length: 0, domain: None }
    }
}

/// A table or feature class to create.
#[derive(Debug, Clone)]
pub struct TableDef {
    /// Name.
    pub name: String,
    /// The geometry, for feature classes.
    pub geometry: Option<GeometryDef>,
    /// The attribute fields, in order. OBJECTID, Shape, Shape_Length and
    /// Shape_Area are added and maintained automatically.
    pub fields: Vec<FieldDef>,
}

// ---------------------------------------------------------------------------
// Low-level table serialisation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawField {
    name: String,
    alias: String,
    field_type: FieldType,
    nullable: bool,
    required: bool,
    editable: bool,
    length: u32,
}

impl RawField {
    fn new(name: &str, field_type: FieldType, nullable: bool, length: u32) -> Self {
        let oid = field_type == FieldType::ObjectId || field_type == FieldType::GlobalId;
        Self { name: name.into(), alias: String::new(), field_type, nullable, required: oid, editable: !oid, length }
    }
}

/// An in-memory `.gdbtable` being built (format version 3, as GDAL writes).
struct RawTable {
    file_id: u32,
    fields: Vec<RawField>,
    geom: Option<GeometryFieldInfo>,
    geom_type_byte: u8,
    offset_size: usize,
    rows: Vec<Vec<u8>>,
    extent: [f64; 4],
    /// User tables get a `.gdbindexes` (and feature classes a `.spx`), as
    /// ArcGIS writes them; system tables do not.
    user: bool,
    /// (OID, quantized shape) of every non-null geometry, for the `.spx`.
    shapes: Vec<(i64, Shape)>,
    /// Spatial index grid size written in the geometry field descriptor.
    grid: f64,
    /// The coordinate system's horizon (xmin, ymin, xmax, ymax), written as the `.horizon`
    /// file of a feature class as ArcGIS Pro does; `None`: not written.
    horizon: Option<[f64; 4]>,
}

fn guid_bytes(s: &str) -> Result<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return Err(Error::InvalidArgument(format!("invalid GUID {s}")));
    }
    let mut b = [0u8; 16];
    for (i, item) in b.iter_mut().enumerate() {
        *item = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| Error::InvalidArgument(format!("invalid GUID {s}")))?;
    }
    // Stored as Windows GUID: first three groups little-endian.
    Ok([b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]])
}

impl RawTable {
    fn new(file_id: u32, geom: Option<GeometryFieldInfo>, offset_size: usize) -> Self {
        let geom_type_byte = match geom.as_ref().map(|g| g.shape_type) {
            None => 0,
            Some(ShapeType::Point) => 1,
            Some(ShapeType::Multipoint) => 2,
            Some(ShapeType::Polyline) => 3,
            Some(ShapeType::Polygon) => 4,
            Some(ShapeType::MultiPatch) => 9,
        };
        Self {
            file_id,
            fields: Vec::new(),
            geom,
            geom_type_byte,
            offset_size,
            rows: Vec::new(),
            extent: [f64::NAN; 4],
            user: false,
            shapes: Vec::new(),
            grid: 0.0,
            horizon: None,
        }
    }

    /// Encodes and appends a row; `values` parallel to `self.fields`
    /// (ObjectId entries ignored, Geometry entries hold the shape). Returns the OID.
    fn insert(&mut self, values: &[Value]) -> Result<i64> {
        let n_nullable = self.fields.iter().filter(|f| f.nullable).count();
        let mut buf = vec![0xFFu8; n_nullable.div_ceil(8)];
        let mut i_null = 0usize;
        for (f, v) in self.fields.iter().zip(values) {
            if f.field_type == FieldType::ObjectId {
                continue;
            }
            let is_null = matches!(v, Value::Null);
            if is_null {
                if !f.nullable {
                    return Err(Error::InvalidArgument(format!("null value in non-nullable field {}", f.name)));
                }
                i_null += 1;
                continue;
            }
            match (f.field_type, v) {
                (FieldType::Int16, _) => {
                    let x = v.as_i64().ok_or_else(|| type_err(f, v))?;
                    let x = i16::try_from(x).map_err(|_| Error::InvalidArgument(format!("{x} out of range for Int16 field {}", f.name)))?;
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                (FieldType::Int32, _) => {
                    let x = v.as_i64().ok_or_else(|| type_err(f, v))?;
                    let x = i32::try_from(x).map_err(|_| Error::InvalidArgument(format!("{x} out of range for Int32 field {}", f.name)))?;
                    buf.extend_from_slice(&x.to_le_bytes());
                }
                (FieldType::Int64, _) => buf.extend_from_slice(&v.as_i64().ok_or_else(|| type_err(f, v))?.to_le_bytes()),
                (FieldType::Float32, Value::Float32(x)) => buf.extend_from_slice(&x.to_le_bytes()),
                (FieldType::Float32, _) => buf.extend_from_slice(&(v.as_f64().ok_or_else(|| type_err(f, v))? as f32).to_le_bytes()),
                (FieldType::Float64, _) => buf.extend_from_slice(&v.as_f64().ok_or_else(|| type_err(f, v))?.to_le_bytes()),
                (FieldType::DateTime | FieldType::Date | FieldType::Time, Value::DateTime(x)) => buf.extend_from_slice(&x.to_le_bytes()),
                (FieldType::String | FieldType::Xml, Value::String(s)) => {
                    write_varuint(&mut buf, s.len() as u64);
                    buf.extend_from_slice(s.as_bytes());
                }
                (FieldType::Guid | FieldType::GlobalId, Value::Guid(s) | Value::String(s)) => buf.extend_from_slice(&guid_bytes(s)?),
                (FieldType::Binary, Value::Binary(b)) => {
                    write_varuint(&mut buf, b.len() as u64);
                    buf.extend_from_slice(b);
                }
                (FieldType::Geometry, Value::Geometry(s)) => {
                    let g = self.geom.as_ref().expect("geometry info");
                    let blob = encode_shape(s, g)?;
                    if let Some(e) = s.envelope() {
                        let x = &mut self.extent;
                        if x[0].is_nan() {
                            *x = e;
                        } else {
                            x[0] = x[0].min(e[0]);
                            x[1] = x[1].min(e[1]);
                            x[2] = x[2].max(e[2]);
                            x[3] = x[3].max(e[3]);
                        }
                    }
                    write_varuint(&mut buf, blob.len() as u64);
                    buf.extend_from_slice(&blob);
                    if self.user {
                        let oid = self.rows.len() as i64 + 1;
                        self.shapes.push((oid, quantized(s, g)));
                    }
                }
                _ => return Err(type_err(f, v)),
            }
            if f.nullable {
                buf[i_null / 8] &= !(1 << (i_null % 8));
                i_null += 1;
            }
        }
        self.rows.push(buf);
        Ok(self.rows.len() as i64)
    }

    fn field_descriptors(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes()); // size, patched below
        b.extend_from_slice(&4u32.to_le_bytes()); // version
        let flags: u32 = u32::from(self.geom_type_byte) | (1 << 8) | (u32::from(self.geom_type_byte != 0) << 9);
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&(self.fields.len() as u16).to_le_bytes());
        for f in &self.fields {
            let name = to_utf16(&f.name);
            b.push((name.len() / 2) as u8);
            b.extend_from_slice(&name);
            let alias = to_utf16(&f.alias);
            b.push((alias.len() / 2) as u8);
            b.extend_from_slice(&alias);
            let ty: u8 = match f.field_type {
                FieldType::Int16 => 0,
                FieldType::Int32 => 1,
                FieldType::Float32 => 2,
                FieldType::Float64 => 3,
                FieldType::String => 4,
                FieldType::DateTime => 5,
                FieldType::ObjectId => 6,
                FieldType::Geometry => 7,
                FieldType::Binary => 8,
                FieldType::Raster => 9,
                FieldType::Guid => 10,
                FieldType::GlobalId => 11,
                FieldType::Xml => 12,
                FieldType::Int64 => 13,
                FieldType::Date => 14,
                FieldType::Time => 15,
                FieldType::DateTimeOffset => 16,
            };
            b.push(ty);
            let flag = u8::from(f.nullable) | (u8::from(f.required) << 1) | (u8::from(f.editable) << 2);
            match f.field_type {
                FieldType::Int16 => b.extend_from_slice(&[2, flag, 0]),
                FieldType::Int32 | FieldType::Float32 => b.extend_from_slice(&[4, flag, 0]),
                FieldType::Float64 | FieldType::DateTime | FieldType::Date | FieldType::Time | FieldType::Int64 => {
                    b.extend_from_slice(&[8, flag, 0])
                }
                FieldType::DateTimeOffset => b.extend_from_slice(&[10, flag, 0]),
                FieldType::String => {
                    b.extend_from_slice(&f.length.to_le_bytes());
                    b.push(flag);
                    b.push(0); // no default value
                }
                FieldType::ObjectId => b.extend_from_slice(&[4, 2]),
                FieldType::Guid | FieldType::GlobalId => b.extend_from_slice(&[38, flag]),
                FieldType::Binary | FieldType::Xml | FieldType::Raster => b.extend_from_slice(&[0, flag]),
                FieldType::Geometry => {
                    let g = self.geom.as_ref().expect("geometry info");
                    b.extend_from_slice(&[0, flag]);
                    let wkt = to_utf16(&g.wkt);
                    b.extend_from_slice(&(wkt.len() as u16).to_le_bytes());
                    b.extend_from_slice(&wkt);
                    b.push(1 | 2 | 4); // has M and Z origin/scale/tolerance
                    for v in [
                        g.x_origin,
                        g.y_origin,
                        g.xy_scale,
                        g.m_origin,
                        g.m_scale,
                        g.z_origin,
                        g.z_scale,
                        g.xy_tolerance,
                        g.m_tolerance,
                        g.z_tolerance,
                    ] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    for v in self.extent {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    b.push(0);
                    b.extend_from_slice(&1u32.to_le_bytes()); // one grid level
                    b.extend_from_slice(&self.grid.to_le_bytes());
                }
            }
        }
        b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let size = (b.len() - 4) as u32;
        b[0..4].copy_from_slice(&size.to_le_bytes());
        b
    }

    fn write(&mut self, dir: &Path) -> Result<()> {
        let base = dir.join(format!("a{:08x}", self.file_id));
        if self.user {
            let oid = self.fields.iter().find(|f| f.field_type == FieldType::ObjectId).map_or("OBJECTID", |f| f.name.as_str());
            let geom = self.fields.iter().find(|f| f.field_type == FieldType::Geometry).map(|f| f.name.as_str());
            if let (Some(_), Some(g)) = (geom, &self.geom) {
                self.grid = spatial_index::grid_size(g.shape_type, &self.shapes, self.extent);
                let p = base.with_extension("spx");
                std::fs::write(&p, spatial_index::spx(self.grid, &self.shapes)).map_err(|e| Error::Io { path: p, source: e })?;
                if let Some(h) = self.horizon {
                    let p = base.with_extension("horizon");
                    let bytes: Vec<u8> = h.iter().flat_map(|v| v.to_le_bytes()).collect();
                    std::fs::write(&p, bytes).map_err(|e| Error::Io { path: p, source: e })?;
                }
            }
            let p = base.with_extension("gdbindexes");
            std::fs::write(&p, spatial_index::gdbindexes(oid, geom)).map_err(|e| Error::Io { path: p, source: e })?;
        }
        let fd = self.field_descriptors();
        let mut t = Vec::with_capacity(40 + fd.len() + self.rows.iter().map(|r| r.len() + 4).sum::<usize>());
        let max_size = self.rows.iter().map(Vec::len).max().unwrap_or(0).max(fd.len() - 4) as u32;
        t.extend_from_slice(&3u32.to_le_bytes());
        t.extend_from_slice(&(self.rows.len() as u32).to_le_bytes());
        t.extend_from_slice(&max_size.to_le_bytes());
        t.extend_from_slice(&5u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&0u64.to_le_bytes()); // file size, patched
        t.extend_from_slice(&40u64.to_le_bytes()); // field descriptor offset
        t.extend_from_slice(&fd);
        let mut offsets = Vec::with_capacity(self.rows.len());
        for r in &self.rows {
            offsets.push(t.len() as u64);
            t.extend_from_slice(&(r.len() as u32).to_le_bytes());
            t.extend_from_slice(r);
        }
        let size = t.len() as u64;
        if size >> (8 * self.offset_size) != 0 {
            return Err(Error::InvalidArgument("table too large for its offset size".into()));
        }
        t[24..32].copy_from_slice(&size.to_le_bytes());

        let n = self.rows.len();
        let blocks = n.div_ceil(1024);
        let mut x = Vec::with_capacity(16 + blocks * 1024 * self.offset_size + 16);
        x.extend_from_slice(&3u32.to_le_bytes());
        x.extend_from_slice(&(blocks as u32).to_le_bytes());
        x.extend_from_slice(&(n as u32).to_le_bytes());
        x.extend_from_slice(&(self.offset_size as u32).to_le_bytes());
        for i in 0..blocks * 1024 {
            let o = offsets.get(i).copied().unwrap_or(0);
            x.extend_from_slice(&o.to_le_bytes()[..self.offset_size]);
        }
        // Trailer: no block map (all blocks present).
        x.extend_from_slice(&0u32.to_le_bytes());
        x.extend_from_slice(&(blocks as u32).to_le_bytes());
        x.extend_from_slice(&(blocks as u32).to_le_bytes());
        x.extend_from_slice(&0u32.to_le_bytes());

        let p = base.with_extension("gdbtable");
        std::fs::write(&p, &t).map_err(|e| Error::Io { path: p, source: e })?;
        let p = base.with_extension("gdbtablx");
        std::fs::write(&p, &x).map_err(|e| Error::Io { path: p, source: e })?;
        Ok(())
    }
}

fn type_err(f: &RawField, v: &Value) -> Error {
    Error::InvalidArgument(format!("value {v:?} does not fit field {} of type {:?}", f.name, f.field_type))
}

/// Encodes a shape, including circular-arc curve segments (as ArcGIS and GDAL
/// do: interior-point arcs, flag 0x80).
fn encode_shape(s: &Shape, g: &GeometryFieldInfo) -> Result<Vec<u8>> {
    if !s.has_curves || s.curves.is_empty() {
        let mut straight = s.clone();
        straight.has_curves = false;
        return geometry::encode(&straight, g);
    }
    if !matches!(s.shape_type, ShapeType::Polygon | ShapeType::Polyline) {
        return Err(Error::Unsupported("curves on point geometries".into()));
    }
    let base = if s.shape_type == ShapeType::Polygon { 51u32 } else { 50u32 };
    let zf = if g.has_z { EXT_SHAPE_Z_FLAG } else { 0 };
    let mf = if g.has_m { EXT_SHAPE_M_FLAG } else { 0 };
    let mut out = Vec::new();
    write_varuint(&mut out, u64::from(base | EXT_SHAPE_CURVE_FLAG | zf | mf));
    let parts: Vec<&Vec<crate::filegdb::Coord>> = s.parts.iter().collect();
    let n_points: usize = parts.iter().map(|p| p.len()).sum();
    write_varuint(&mut out, n_points as u64);
    write_varuint(&mut out, parts.len() as u64);
    write_varuint(&mut out, s.curves.len() as u64);
    // Envelope as GDAL encodes it (true envelope, rounded half up).
    let e = s.envelope().ok_or_else(|| Error::InvalidArgument("empty curved shape".into()))?;
    for v in [(e[0] - g.x_origin) * g.xy_scale, (e[1] - g.y_origin) * g.xy_scale, (e[2] - e[0]) * g.xy_scale, (e[3] - e[1]) * g.xy_scale] {
        write_varuint(&mut out, (v + 0.5) as u64);
    }
    for p in &parts[..parts.len() - 1] {
        write_varuint(&mut out, p.len() as u64);
    }
    let (mut px, mut py) = (0i64, 0i64);
    for c in parts.iter().flat_map(|p| p.iter()) {
        let x = ((c.x - g.x_origin) * g.xy_scale).round() as i64;
        let y = ((c.y - g.y_origin) * g.xy_scale).round() as i64;
        write_varint(&mut out, x - px);
        write_varint(&mut out, y - py);
        px = x;
        py = y;
    }
    for c in &s.curves {
        match *c {
            CurveSegment::ArcInteriorPoint { start_index, x, y, ccw } => {
                write_varuint(&mut out, start_index as u64);
                out.push(1);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&(0x80u32 | if ccw { 0x8 } else { 0 }).to_le_bytes());
            }
            CurveSegment::ArcCenterPoint { start_index, center_x, center_y, ccw } => {
                write_varuint(&mut out, start_index as u64);
                out.push(1);
                out.extend_from_slice(&center_x.to_le_bytes());
                out.extend_from_slice(&center_y.to_le_bytes());
                out.extend_from_slice(&(if ccw { 0x8u32 } else { 0 }).to_le_bytes());
            }
            CurveSegment::Bezier { start_index, x1, y1, x2, y2 } => {
                write_varuint(&mut out, start_index as u64);
                out.push(4);
                for v in [x1, y1, x2, y2] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            CurveSegment::Ellipse { start_index, params, bits } => {
                write_varuint(&mut out, start_index as u64);
                out.push(5);
                for v in params {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&bits.to_le_bytes());
            }
        }
    }
    Ok(out)
}

/// Quantizes a shape to the grid of `g` (what a write/read round trip yields).
fn quantized(s: &Shape, g: &GeometryFieldInfo) -> Shape {
    let q = |v: f64, o: f64| (((v - o) * g.xy_scale).round()) / g.xy_scale + o;
    let mut out = s.clone();
    for p in &mut out.parts {
        for c in p.iter_mut() {
            c.x = q(c.x, g.x_origin);
            c.y = q(c.y, g.y_origin);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// UUIDs and XML
// ---------------------------------------------------------------------------

struct UuidGen {
    state: u64,
}

impl UuidGen {
    fn new() -> Self {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        Self { state: h.finish() | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A random (version 4) UUID in Esri's upper-case braced form.
    fn uuid(&mut self) -> String {
        let a = self.next_u64();
        let b = self.next_u64();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&a.to_be_bytes());
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        let h: String = bytes.iter().map(|x| format!("{x:02X}")).collect();
        format!("{{{}-{}-{}-{}-{}}}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

const XMLNS: &str = "xmlns:xsi='http://www.w3.org/2001/XMLSchema-instance' xmlns:xs='http://www.w3.org/2001/XMLSchema' xmlns:typens='http://www.esri.com/schemas/ArcGIS/10.8'";

fn esri_field_type(t: FieldType) -> &'static str {
    match t {
        FieldType::Int16 => "esriFieldTypeSmallInteger",
        FieldType::Int32 => "esriFieldTypeInteger",
        FieldType::Int64 => "esriFieldTypeBigInteger",
        FieldType::Float32 => "esriFieldTypeSingle",
        FieldType::Float64 => "esriFieldTypeDouble",
        FieldType::String => "esriFieldTypeString",
        FieldType::DateTime => "esriFieldTypeDate",
        FieldType::ObjectId => "esriFieldTypeOID",
        FieldType::Geometry => "esriFieldTypeGeometry",
        FieldType::Binary => "esriFieldTypeBlob",
        FieldType::Raster => "esriFieldTypeRaster",
        FieldType::Guid => "esriFieldTypeGUID",
        FieldType::GlobalId => "esriFieldTypeGlobalID",
        FieldType::Xml => "esriFieldTypeXML",
        FieldType::Date => "esriFieldTypeDateOnly",
        FieldType::Time => "esriFieldTypeTimeOnly",
        FieldType::DateTimeOffset => "esriFieldTypeTimestampOffset",
    }
}

fn xml_number(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:?}")
    }
}

fn sr_xml(g: &GeometryDef, gi: &GeometryFieldInfo) -> String {
    let mut s = format!(
        "<SpatialReference xsi:type='typens:ProjectedCoordinateSystem'><WKT>{}</WKT><XOrigin>{}</XOrigin><YOrigin>{}</YOrigin><XYScale>{}</XYScale><ZOrigin>{}</ZOrigin><ZScale>{}</ZScale><MOrigin>{}</MOrigin><MScale>{}</MScale><XYTolerance>{}</XYTolerance><ZTolerance>{}</ZTolerance><MTolerance>{}</MTolerance><HighPrecision>true</HighPrecision>",
        xml_escape(&g.wkt),
        xml_number(gi.x_origin),
        xml_number(gi.y_origin),
        xml_number(gi.xy_scale),
        xml_number(gi.z_origin),
        xml_number(gi.z_scale),
        xml_number(gi.m_origin),
        xml_number(gi.m_scale),
        xml_number(gi.xy_tolerance),
        xml_number(gi.z_tolerance),
        xml_number(gi.m_tolerance)
    );
    if let Some(w) = g.wkid {
        s.push_str(&format!("<WKID>{w}</WKID>"));
    }
    if let Some(w) = g.latest_wkid {
        s.push_str(&format!("<LatestWKID>{w}</LatestWKID>"));
    }
    s.push_str("</SpatialReference>");
    if !g.wkt.starts_with("PROJCS") {
        s = s.replace("typens:ProjectedCoordinateSystem", "typens:GeographicCoordinateSystem");
    }
    s
}

// ---------------------------------------------------------------------------
// The geodatabase writer
// ---------------------------------------------------------------------------

struct Domain {
    name: String,
    uuid: String,
}

struct UserTable {
    def: TableDef,
    raw: RawTable,
    uuid: String,
    dsid: i64,
    /// Index in raw.fields of each def field.
    map: Vec<usize>,
    geom_idx: Option<usize>,
    len_idx: Option<usize>,
    area_idx: Option<usize>,
}

/// Writer of a new File Geodatabase.
///
/// ```no_run
/// use terrio::filegdb::*;
/// let mut w = GdbWriter::create("out.gdb")?;
/// w.add_coded_domain("STATUS", "STATUS", FieldType::String, &[("A", "A"), ("B", "B")])?;
/// let mut f = FieldDef::new("STATUS", FieldType::String);
/// f.length = 50;
/// f.domain = Some("STATUS".into());
/// w.create_table(&TableDef { name: "Areas".into(), geometry: Some(GeometryDef::epsg_2278(ShapeType::Polygon)), fields: vec![f] })?;
/// let square = Shape::new(ShapeType::Polygon, vec![vec![Coord::xy(0.0, 0.0), Coord::xy(0.0, 1.0), Coord::xy(1.0, 1.0), Coord::xy(1.0, 0.0), Coord::xy(0.0, 0.0)]]);
/// w.insert("Areas", Some(&square), vec![Value::String("A".into())])?;
/// w.finish()?;
/// # Ok::<(), terrio::filegdb::Error>(())
/// ```
pub struct GdbWriter {
    dir: PathBuf,
    uuids: UuidGen,
    root_uuid: String,
    workspace_uuid: String,
    domains: Vec<Domain>,
    domain_items: Vec<(String, String, String)>, // uuid, name, xml
    tables: Vec<UserTable>,
    spatial_refs: Vec<(GeometryFieldInfo, String)>,
    next_file_id: u32,
}

impl GdbWriter {
    /// Starts a new geodatabase at `path` (a folder ending in `.gdb`, which
    /// must not exist yet).
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        if !dir.extension().is_some_and(|e| e.eq_ignore_ascii_case("gdb")) {
            return Err(Error::InvalidArgument(format!("{} does not end in .gdb", dir.display())));
        }
        if dir.exists() {
            return Err(Error::InvalidArgument(format!("{} already exists", dir.display())));
        }
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io { path: dir.clone(), source: e })?;
        let mut uuids = UuidGen::new();
        let root_uuid = uuids.uuid();
        let workspace_uuid = uuids.uuid();
        Ok(Self {
            dir,
            uuids,
            root_uuid,
            workspace_uuid,
            domains: Vec::new(),
            domain_items: Vec::new(),
            tables: Vec::new(),
            spatial_refs: Vec::new(),
            next_file_id: 9,
        })
    }

    /// Adds a coded-value domain. `values` are (code, description) pairs;
    /// codes are parsed as numbers for numeric field types.
    pub fn add_coded_domain(&mut self, name: &str, description: &str, field_type: FieldType, values: &[(&str, &str)]) -> Result<()> {
        if self.domains.iter().any(|d| d.name.eq_ignore_ascii_case(name)) {
            return Err(Error::InvalidArgument(format!("domain {name} already exists")));
        }
        let code_type = match field_type {
            FieldType::String => "xs:string",
            FieldType::Int16 => "xs:short",
            FieldType::Int32 => "xs:int",
            FieldType::Int64 => "xs:long",
            FieldType::Float32 => "xs:float",
            FieldType::Float64 => "xs:double",
            other => return Err(Error::Unsupported(format!("domains of type {other:?}"))),
        };
        let mut xml = format!(
            "<GPCodedValueDomain2 xsi:type='typens:GPCodedValueDomain2' {XMLNS}><DomainName>{}</DomainName><FieldType>{}</FieldType><MergePolicy>esriMPTDefaultValue</MergePolicy><SplitPolicy>esriSPTDefaultValue</SplitPolicy><Description>{}</Description><Owner></Owner><CodedValues xsi:type='typens:ArrayOfCodedValue'>",
            xml_escape(name),
            esri_field_type(field_type),
            xml_escape(description)
        );
        for (code, desc) in values {
            xml.push_str(&format!(
                "<CodedValue xsi:type='typens:CodedValue'><Name>{}</Name><Code xsi:type='{code_type}'>{}</Code></CodedValue>",
                xml_escape(desc),
                xml_escape(code)
            ));
        }
        xml.push_str("</CodedValues></GPCodedValueDomain2>");
        let uuid = self.uuids.uuid();
        self.domain_items.push((uuid.clone(), name.to_string(), xml));
        self.domains.push(Domain { name: name.to_string(), uuid });
        Ok(())
    }

    /// Creates a table, or a feature class if `def.geometry` is set.
    pub fn create_table(&mut self, def: &TableDef) -> Result<()> {
        if self.tables.iter().any(|t| t.def.name.eq_ignore_ascii_case(&def.name)) || def.name.to_ascii_uppercase().starts_with("GDB_") {
            return Err(Error::InvalidArgument(format!("table name {} is taken", def.name)));
        }
        if def.name.is_empty()
            || def.name.len() > 160
            || !def.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || def.name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(Error::InvalidArgument(format!("invalid table name {}", def.name)));
        }
        for f in &def.fields {
            if let Some(d) = &f.domain
                && !self.domains.iter().any(|x| &x.name == d)
            {
                return Err(Error::InvalidArgument(format!("unknown domain {d} on field {}", f.name)));
            }
            if matches!(f.field_type, FieldType::ObjectId | FieldType::Geometry | FieldType::Raster) {
                return Err(Error::InvalidArgument(format!("field {} has a managed type", f.name)));
            }
            let reserved = ["OBJECTID", "SHAPE", "SHAPE_LENGTH", "SHAPE_AREA"];
            if reserved.contains(&f.name.to_ascii_uppercase().as_str()) {
                return Err(Error::InvalidArgument(format!("field name {} is managed", f.name)));
            }
        }
        let gi = def.geometry.as_ref().map(GeometryDef::info);
        let file_id = self.next_file_id;
        self.next_file_id += 1;
        let mut raw = RawTable::new(file_id, gi.clone(), 5);
        raw.user = true;
        raw.horizon = def.geometry.as_ref().and_then(GeometryDef::horizon);
        raw.fields.push(RawField::new("OBJECTID", FieldType::ObjectId, false, 4));
        let mut geom_idx = None;
        if gi.is_some() {
            geom_idx = Some(raw.fields.len());
            raw.fields.push(RawField { required: true, ..RawField::new("Shape", FieldType::Geometry, true, 0) });
        }
        let mut map = Vec::new();
        for f in &def.fields {
            map.push(raw.fields.len());
            let len = match f.field_type {
                FieldType::String => f.length.max(1),
                _ => 0,
            };
            let mut rf = RawField::new(&f.name, f.field_type, f.nullable, len);
            rf.alias = f.alias.clone();
            raw.fields.push(rf);
        }
        let (mut len_idx, mut area_idx) = (None, None);
        if let Some(g) = &def.geometry {
            if matches!(g.shape_type, ShapeType::Polyline | ShapeType::Polygon) {
                len_idx = Some(raw.fields.len());
                raw.fields.push(RawField { required: true, editable: false, ..RawField::new("Shape_Length", FieldType::Float64, true, 8) });
            }
            if g.shape_type == ShapeType::Polygon {
                area_idx = Some(raw.fields.len());
                raw.fields.push(RawField { required: true, editable: false, ..RawField::new("Shape_Area", FieldType::Float64, true, 8) });
            }
            if let Some(gi) = &gi
                && !self.spatial_refs.iter().any(|(s, _)| s.wkt == gi.wkt && s.x_origin == gi.x_origin && s.xy_scale == gi.xy_scale)
            {
                self.spatial_refs.push((gi.clone(), g.wkt.clone()));
            }
        }
        let uuid = self.uuids.uuid();
        self.tables.push(UserTable { def: def.clone(), raw, uuid, dsid: 0, map, geom_idx, len_idx, area_idx });
        Ok(())
    }

    /// Inserts a row. `values` are parallel to the table's `def.fields`;
    /// `shape` is the geometry (feature classes only; `None` = null shape).
    /// Shape_Length and Shape_Area are computed from the shape quantized to
    /// the coordinate grid (0 for a null shape, as ArcGIS stores them).
    /// Returns the new OBJECTID.
    pub fn insert(&mut self, table: &str, shape: Option<&Shape>, values: Vec<Value>) -> Result<i64> {
        let t =
            self.tables.iter_mut().find(|t| t.def.name.eq_ignore_ascii_case(table)).ok_or_else(|| Error::NotFound(table.to_string()))?;
        if values.len() != t.def.fields.len() {
            return Err(Error::InvalidArgument(format!("{} values for {} fields of {}", values.len(), t.def.fields.len(), t.def.name)));
        }
        let mut row = vec![Value::Null; t.raw.fields.len()];
        for (v, &i) in values.into_iter().zip(&t.map) {
            row[i] = v;
        }
        if let (Some(gi), Some(s)) = (t.geom_idx, shape) {
            let g = t.raw.geom.as_ref().expect("geometry");
            if s.shape_type != g.shape_type {
                return Err(Error::InvalidArgument(format!("{:?} shape in {:?} feature class {}", s.shape_type, g.shape_type, t.def.name)));
            }
            let q = quantized(s, g);
            if let Some(i) = t.len_idx {
                row[i] = Value::Float64(q.length());
            }
            if let Some(i) = t.area_idx {
                row[i] = Value::Float64(q.area());
            }
            row[gi] = Value::Geometry(s.clone());
        } else if shape.is_some() {
            return Err(Error::InvalidArgument(format!("{} has no geometry", t.def.name)));
        } else {
            // ArcGIS stores 0 (not null) Shape_Length/Shape_Area for a null shape.
            for i in [t.len_idx, t.area_idx].into_iter().flatten() {
                row[i] = Value::Float64(0.0);
            }
        }
        t.raw.insert(&row)
    }

    /// Writes the geodatabase.
    pub fn finish(mut self) -> Result<()> {
        let dir = self.dir.clone();
        let w = |name: &str, bytes: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).map_err(|e| Error::Io { path: p, source: e })
        };
        // What the FileGDB SDK writes.
        w("gdb", b"\x05\x00\x00\x00\xDE\xAD\xBE\xEF")?;
        w("timestamps", &[0xFFu8; 400])?;

        // GDB_SystemCatalog.
        let mut cat = RawTable::new(1, None, 4);
        cat.fields = vec![
            RawField::new("ID", FieldType::ObjectId, false, 4),
            RawField::new("Name", FieldType::String, false, 160),
            RawField::new("FileFormat", FieldType::Int32, false, 0),
        ];
        let mut names: Vec<(String, i64)> = [
            "GDB_SystemCatalog",
            "GDB_DBTune",
            "GDB_SpatialRefs",
            "GDB_Items",
            "GDB_ItemTypes",
            "GDB_ItemRelationships",
            "GDB_ItemRelationshipTypes",
        ]
        .iter()
        .map(|s| (s.to_string(), 0))
        .collect();
        names.push(("GDB_ReplicaLog".into(), 2));
        for t in &self.tables {
            names.push((t.def.name.clone(), 0));
        }
        for (n, ff) in names {
            cat.insert(&[Value::Null, Value::String(n), Value::Int(ff)])?;
        }
        cat.write(&dir)?;

        // GDB_DBTune.
        let mut tune = RawTable::new(2, None, 4);
        tune.fields = vec![
            RawField::new("Keyword", FieldType::String, false, 32),
            RawField::new("ParameterName", FieldType::String, false, 32),
            RawField::new("ConfigString", FieldType::String, true, 2048),
        ];
        for (k, p, c) in DBTUNE {
            tune.insert(&[Value::String((*k).into()), Value::String((*p).into()), Value::String((*c).into())])?;
        }
        tune.write(&dir)?;

        // GDB_SpatialRefs.
        let mut srs = RawTable::new(3, None, 4);
        srs.fields = vec![RawField::new("ID", FieldType::ObjectId, false, 4), RawField::new("SRTEXT", FieldType::String, false, 2048)];
        for n in ["FalseX", "FalseY", "XYUnits", "FalseZ", "ZUnits", "FalseM", "MUnits", "XYTolerance", "ZTolerance", "MTolerance"] {
            srs.fields.push(RawField::new(n, FieldType::Float64, true, 0));
        }
        let items_geom = GeometryFieldInfo {
            shape_type: ShapeType::Polygon,
            has_z: false,
            has_m: false,
            wkt: ESRI_WKT_WGS84.into(),
            x_origin: -180.0,
            y_origin: -90.0,
            xy_scale: 1_000_000.0,
            z_origin: -100_000.0,
            z_scale: 10_000.0,
            m_origin: -100_000.0,
            m_scale: 10_000.0,
            xy_tolerance: 0.000_002,
            z_tolerance: 0.001,
            m_tolerance: 0.001,
            extent: [f64::NAN; 4],
        };
        let mut all_srs = vec![items_geom.clone()];
        all_srs.extend(self.spatial_refs.iter().map(|(g, _)| g.clone()));
        for g in &all_srs {
            srs.insert(&[
                Value::Null,
                Value::String(g.wkt.clone()),
                Value::Float64(g.x_origin),
                Value::Float64(g.y_origin),
                Value::Float64(g.xy_scale),
                Value::Float64(g.z_origin),
                Value::Float64(g.z_scale),
                Value::Float64(g.m_origin),
                Value::Float64(g.m_scale),
                Value::Float64(g.xy_tolerance),
                Value::Float64(g.z_tolerance),
                Value::Float64(g.m_tolerance),
            ])?;
        }
        srs.write(&dir)?;

        // GDB_Items.
        let mut items = RawTable::new(4, Some(items_geom), 4);
        items.fields = vec![
            RawField::new("ObjectID", FieldType::ObjectId, false, 4),
            RawField::new("UUID", FieldType::GlobalId, false, 38),
            RawField::new("Type", FieldType::Guid, false, 38),
            RawField::new("Name", FieldType::String, true, 160),
            RawField::new("PhysicalName", FieldType::String, true, 160),
            RawField::new("Path", FieldType::String, true, 260),
            RawField::new("DatasetSubtype1", FieldType::Int32, true, 0),
            RawField::new("DatasetSubtype2", FieldType::Int32, true, 0),
            RawField::new("DatasetInfo1", FieldType::String, true, 255),
            RawField::new("DatasetInfo2", FieldType::String, true, 255),
            RawField::new("URL", FieldType::String, true, 255),
            RawField::new("Definition", FieldType::Xml, true, 0),
            RawField::new("Documentation", FieldType::Xml, true, 0),
            RawField::new("ItemInfo", FieldType::Xml, true, 0),
            RawField::new("Properties", FieldType::Int32, true, 0),
            RawField::new("Defaults", FieldType::Binary, true, 0),
            RawField::new("Shape", FieldType::Geometry, true, 0),
        ];
        let s = |v: &str| Value::String(v.to_string());
        let item = |uuid: &str,
                    ty: &str,
                    name: &str,
                    phys: &str,
                    path: &str,
                    st: Option<(i64, i64)>,
                    info1: Option<&str>,
                    def: Option<String>,
                    props: i64|
         -> Vec<Value> {
            vec![
                Value::Null,
                Value::Guid(uuid.into()),
                Value::Guid(ty.into()),
                s(name),
                s(phys),
                s(path),
                st.map_or(Value::Null, |x| Value::Int(x.0)),
                st.map_or(Value::Null, |x| Value::Int(x.1)),
                info1.map_or(Value::Null, s),
                Value::Null,
                s(""),
                def.map_or(Value::Null, Value::String),
                Value::Null,
                Value::Null,
                Value::Int(props),
                Value::Null,
                Value::Null,
            ]
        };
        items.insert(&item(&self.root_uuid, FOLDER_TYPE, "", "", "\\", None, None, None, 1))?;
        let ws = format!(
            "<DEWorkspace xsi:type='typens:DEWorkspace' {XMLNS}><CatalogPath>\\</CatalogPath><Name></Name><ChildrenExpanded>false</ChildrenExpanded><WorkspaceType>esriLocalDatabaseWorkspace</WorkspaceType><WorkspaceFactoryProgID></WorkspaceFactoryProgID><ConnectionString></ConnectionString><ConnectionInfo xsi:nil='true'/><Domains xsi:type='typens:ArrayOfDomain'></Domains><MajorVersion>3</MajorVersion><MinorVersion>0</MinorVersion><BugfixVersion>0</BugfixVersion><Realm></Realm><MaxAttributeRuleID>0</MaxAttributeRuleID></DEWorkspace>"
        );
        let ws_uuid = self.workspace_uuid.clone();
        items.insert(&item(&ws_uuid, WORKSPACE_TYPE, "Workspace", "WORKSPACE", "", None, None, Some(ws), 0))?;
        for (uuid, name, xml) in &self.domain_items {
            items.insert(&item(uuid, CODED_DOMAIN_TYPE, name, &name.to_ascii_uppercase(), "", None, None, Some(xml.clone()), 1))?;
        }
        let mut rels: Vec<(String, String, &str)> = Vec::new();
        let first_dsid = items.rows.len() as i64 + 1;
        for (k, t) in self.tables.iter_mut().enumerate() {
            t.dsid = first_dsid + k as i64;
        }
        for t in &mut self.tables {
            t.raw.write(&dir)?;
        }
        for t in &self.tables {
            let xml = table_xml(t);
            let path = format!("\\{}", t.def.name);
            let (ty, st, info1) = match &t.def.geometry {
                Some(_) => (FEATURE_CLASS_TYPE, Some((1, i64::from(t.raw.geom_type_byte))), Some("Shape")),
                None => (TABLE_TYPE, None, None),
            };
            items.insert(&item(&t.uuid, ty, &t.def.name, &t.def.name.to_ascii_uppercase(), &path, st, info1, Some(xml), 1))?;
            rels.push((self.root_uuid.clone(), t.uuid.clone(), DATASET_IN_FOLDER));
            let used: BTreeSet<&String> = t.def.fields.iter().filter_map(|f| f.domain.as_ref()).collect();
            for d in used {
                let du = self.domains.iter().find(|x| &x.name == d).expect("domain").uuid.clone();
                rels.push((t.uuid.clone(), du, DOMAIN_IN_DATASET));
            }
        }
        items.write(&dir)?;

        // GDB_ItemTypes.
        let mut types = RawTable::new(5, None, 4);
        types.fields = vec![
            RawField::new("ObjectID", FieldType::ObjectId, false, 4),
            RawField::new("UUID", FieldType::Guid, false, 38),
            RawField::new("ParentTypeID", FieldType::Guid, false, 38),
            RawField::new("Name", FieldType::String, false, 160),
        ];
        for (u, p, n) in ITEM_TYPES {
            types.insert(&[Value::Null, Value::Guid((*u).into()), Value::Guid((*p).into()), s(n)])?;
        }
        types.write(&dir)?;

        // GDB_ItemRelationships.
        let mut ir = RawTable::new(6, None, 4);
        ir.fields = vec![
            RawField::new("ObjectID", FieldType::ObjectId, false, 4),
            RawField::new("UUID", FieldType::GlobalId, false, 38),
            RawField::new("OriginID", FieldType::Guid, false, 38),
            RawField::new("DestID", FieldType::Guid, false, 38),
            RawField::new("Type", FieldType::Guid, false, 38),
            RawField::new("Attributes", FieldType::Xml, true, 0),
            RawField::new("Properties", FieldType::Int32, true, 0),
        ];
        for (o, d, t) in rels {
            let u = self.uuids.uuid();
            ir.insert(&[Value::Null, Value::Guid(u), Value::Guid(o), Value::Guid(d), Value::Guid(t.into()), Value::Null, Value::Int(1)])?;
        }
        ir.write(&dir)?;

        // GDB_ItemRelationshipTypes.
        let mut irt = RawTable::new(7, None, 4);
        irt.fields = vec![
            RawField::new("ObjectID", FieldType::ObjectId, false, 4),
            RawField::new("UUID", FieldType::Guid, false, 38),
            RawField::new("OrigItemTypeID", FieldType::Guid, false, 38),
            RawField::new("DestItemTypeID", FieldType::Guid, false, 38),
            RawField::new("Name", FieldType::String, true, 160),
            RawField::new("ForwardLabel", FieldType::String, true, 255),
            RawField::new("BackwardLabel", FieldType::String, true, 255),
            RawField::new("IsContainment", FieldType::Int16, true, 0),
        ];
        for (u, o, d, n, f, b, c) in REL_TYPES {
            irt.insert(&[
                Value::Null,
                Value::Guid((*u).into()),
                Value::Guid((*o).into()),
                Value::Guid((*d).into()),
                s(n),
                s(f),
                s(b),
                Value::Int(*c),
            ])?;
        }
        irt.write(&dir)?;
        Ok(())
    }
}

fn table_xml(t: &UserTable) -> String {
    let is_fc = t.def.geometry.is_some();
    let root = if is_fc { "DEFeatureClassInfo" } else { "DETableInfo" };
    let mut x = format!(
        "<{root} xsi:type='typens:{root}' {XMLNS}><CatalogPath>\\{name}</CatalogPath><Name>{name}</Name><ChildrenExpanded>false</ChildrenExpanded><DatasetType>{dt}</DatasetType><DSID>{dsid}</DSID><Versioned>false</Versioned><CanVersion>false</CanVersion><ConfigurationKeyword></ConfigurationKeyword><RequiredGeodatabaseClientVersion>10.0</RequiredGeodatabaseClientVersion><HasOID>true</HasOID><OIDFieldName>OBJECTID</OIDFieldName><GPFieldInfoExs xsi:type='typens:ArrayOfGPFieldInfoEx'>",
        name = xml_escape(&t.def.name),
        dt = if is_fc { "esriDTFeatureClass" } else { "esriDTTable" },
        dsid = t.dsid
    );
    for (i, f) in t.raw.fields.iter().enumerate() {
        x.push_str("<GPFieldInfoEx xsi:type='typens:GPFieldInfoEx'>");
        x.push_str(&format!("<Name>{}</Name>", xml_escape(&f.name)));
        let user = t.map.iter().position(|&m| m == i).map(|k| &t.def.fields[k]);
        if let Some(u) = user {
            if !u.alias.is_empty() {
                x.push_str(&format!("<AliasName>{}</AliasName>", xml_escape(&u.alias)));
            }
            if let Some(d) = &u.domain {
                x.push_str(&format!("<DomainName>{}</DomainName>", xml_escape(d)));
            }
        }
        x.push_str(&format!("<FieldType>{}</FieldType><IsNullable>{}</IsNullable>", esri_field_type(f.field_type), f.nullable));
        if f.field_type == FieldType::ObjectId {
            x.push_str("<Required>true</Required><Editable>false</Editable>");
        } else if f.field_type == FieldType::Geometry {
            x.push_str("<Required>true</Required>");
        } else if Some(i) == t.len_idx || Some(i) == t.area_idx {
            x.push_str("<Required>true</Required><Editable>false</Editable>");
        }
        x.push_str("</GPFieldInfoEx>");
    }
    x.push_str(&format!(
        "</GPFieldInfoExs><CLSID>{}</CLSID><EXTCLSID></EXTCLSID><RelationshipClassNames xsi:type='typens:Names'></RelationshipClassNames><AliasName></AliasName><ModelName></ModelName><HasGlobalID>false</HasGlobalID><GlobalIDFieldName></GlobalIDFieldName><RasterFieldName></RasterFieldName><ExtensionProperties xsi:type='typens:PropertySet'><PropertyArray xsi:type='typens:ArrayOfPropertySetProperty'></PropertyArray></ExtensionProperties><ControllerMemberships xsi:type='typens:ArrayOfControllerMembership'></ControllerMemberships><EditorTrackingEnabled>false</EditorTrackingEnabled><CreatorFieldName></CreatorFieldName><CreatedAtFieldName></CreatedAtFieldName><EditorFieldName></EditorFieldName><EditedAtFieldName></EditedAtFieldName><IsTimeInUTC>true</IsTimeInUTC>",
        if is_fc { "{52353152-891A-11D0-BEC6-00805F7C4268}" } else { "{7A566981-C114-11D2-8A28-006097AFF44E}" }
    ));
    if let (Some(g), Some(gi)) = (&t.def.geometry, &t.raw.geom) {
        let shape = match g.shape_type {
            ShapeType::Point => "esriGeometryPoint",
            ShapeType::Multipoint => "esriGeometryMultipoint",
            ShapeType::Polyline => "esriGeometryPolyline",
            ShapeType::Polygon => "esriGeometryPolygon",
            ShapeType::MultiPatch => "esriGeometryMultiPatch",
        };
        let e = t.raw.extent;
        let sr = sr_xml(g, gi);
        x.push_str(&format!(
            "<FeatureType>esriFTSimple</FeatureType><ShapeType>{shape}</ShapeType><ShapeFieldName>Shape</ShapeFieldName><HasM>false</HasM><HasZ>false</HasZ><HasSpatialIndex>false</HasSpatialIndex><AreaFieldName>{}</AreaFieldName><LengthFieldName>{}</LengthFieldName><Extent xsi:type='typens:EnvelopeN'><XMin>{}</XMin><YMin>{}</YMin><XMax>{}</XMax><YMax>{}</YMax>{sr}</Extent>{sr}",
            if t.area_idx.is_some() { "Shape_Area" } else { "" },
            if t.len_idx.is_some() { "Shape_Length" } else { "" },
            xml_number(e[0]),
            xml_number(e[1]),
            xml_number(e[2]),
            xml_number(e[3]),
        ));
    }
    x.push_str(&format!(
        "<ChangeTracked>false</ChangeTracked><FieldFilteringEnabled>false</FieldFilteringEnabled><FilteredFieldNames xsi:type='typens:Names'></FilteredFieldNames><ReplicaTracked>false</ReplicaTracked></{root}>"
    ));
    x
}

const DBTUNE: &[(&str, &str, &str)] = &[
    ("DEFAULTS", "UI_TEXT", "The default datafile configuration."),
    ("DEFAULTS", "CHARACTER_FORMAT", "UTF8"),
    ("DEFAULTS", "GEOMETRY_FORMAT", "Compressed"),
    ("DEFAULTS", "GEOMETRY_STORAGE", "InLine"),
    ("DEFAULTS", "BLOB_STORAGE", "InLine"),
    ("DEFAULTS", "MAX_FILE_SIZE", "1TB"),
    ("DEFAULTS", "RASTER_STORAGE", "InLine"),
    ("TEXT_UTF16", "UI_TEXT", "The UTF16 text format configuration."),
    ("TEXT_UTF16", "CHARACTER_FORMAT", "UTF16"),
    ("MAX_FILE_SIZE_4GB", "UI_TEXT", "The 4GB maximum datafile size configuration."),
    ("MAX_FILE_SIZE_4GB", "MAX_FILE_SIZE", "4GB"),
    ("MAX_FILE_SIZE_256TB", "UI_TEXT", "The 256TB maximum datafile size configuration."),
    ("MAX_FILE_SIZE_256TB", "MAX_FILE_SIZE", "256TB"),
    ("GEOMETRY_UNCOMPRESSED", "UI_TEXT", "The Uncompressed Geometry configuration."),
    ("GEOMETRY_UNCOMPRESSED", "GEOMETRY_FORMAT", "Uncompressed"),
    ("GEOMETRY_OUTOFLINE", "UI_TEXT", "The Outofline Geometry configuration."),
    ("GEOMETRY_OUTOFLINE", "GEOMETRY_STORAGE", "OutOfLine"),
    ("BLOB_OUTOFLINE", "UI_TEXT", "The Outofline Blob configuration."),
    ("BLOB_OUTOFLINE", "BLOB_STORAGE", "OutOfLine"),
    ("GEOMETRY_AND_BLOB_OUTOFLINE", "UI_TEXT", "The Outofline Geometry and Blob configuration."),
    ("GEOMETRY_AND_BLOB_OUTOFLINE", "GEOMETRY_STORAGE", "OutOfLine"),
    ("GEOMETRY_AND_BLOB_OUTOFLINE", "BLOB_STORAGE", "OutOfLine"),
    ("TERRAIN_DEFAULTS", "UI_TERRAIN_TEXT", "The terrains default configuration."),
    ("TERRAIN_DEFAULTS", "GEOMETRY_STORAGE", "OutOfLine"),
    ("TERRAIN_DEFAULTS", "BLOB_STORAGE", "OutOfLine"),
    ("MOSAICDATASET_DEFAULTS", "UI_MOSAIC_TEXT", "The Outofline Raster and Blob configuration."),
    ("MOSAICDATASET_DEFAULTS", "RASTER_STORAGE", "OutOfLine"),
    ("MOSAICDATASET_DEFAULTS", "BLOB_STORAGE", "OutOfLine"),
    ("MOSAICDATASET_INLINE", "UI_MOSAIC_TEXT", "The mosaic dataset inline configuration."),
    ("MOSAICDATASET_INLINE", "CHARACTER_FORMAT", "UTF8"),
    ("MOSAICDATASET_INLINE", "GEOMETRY_FORMAT", "Compressed"),
    ("MOSAICDATASET_INLINE", "GEOMETRY_STORAGE", "InLine"),
    ("MOSAICDATASET_INLINE", "BLOB_STORAGE", "InLine"),
    ("MOSAICDATASET_INLINE", "MAX_FILE_SIZE", "1TB"),
    ("MOSAICDATASET_INLINE", "RASTER_STORAGE", "InLine"),
];

/// The item types of a file geodatabase as ArcGIS Pro 3.6 writes them (content and order).
/// ArcGIS adds any missing type to a geodatabase it opens, which rewrites the file: keep
/// this list complete.
const ITEM_TYPES: &[(&str, &str, &str)] = &[
    ("{5b966567-fb87-4dde-938b-b4b37423539d}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Sync Replica"),
    ("{77292603-930f-475d-ae4f-b8970f42f394}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Extension Dataset"),
    ("{8405add5-8df8-4227-8fac-3fcade073386}", "{00000000-0000-0000-0000-000000000000}", "Item"),
    ("{35b601f7-45ce-4aff-adb7-7702d3839b12}", "{d4912162-3413-476e-9da4-2aefbbc16939}", "Raster Catalog"),
    ("{b606a7e1-fa5b-439c-849c-6e9c2481537b}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Relationship Class"),
    ("{a300008d-0cea-4f6a-9dfa-46af829a3df2}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Representation Class"),
    ("{d4912162-3413-476e-9da4-2aefbbc16939}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "AbstractTable"),
    ("{ebeee2c9-fa73-4bed-ac7d-aee7d68afc80}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Parcel Dataset"),
    ("{8637f1ed-8c04-4866-a44a-1cb8288b3c63}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Domain"),
    ("{ffd09c28-fe70-4e25-907c-af8e8a5ec5f3}", "{8405add5-8df8-4227-8fac-3fcade073386}", "Resource"),
    ("{74737149-dcb5-4257-8904-b9724e32a530}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Feature Dataset"),
    ("{f8413dcb-2248-4935-bfe9-315f397e5110}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Mosaic Dataset"),
    ("{5ed667a3-9ca9-44a2-8029-d95bf23704b9}", "{d4912162-3413-476e-9da4-2aefbbc16939}", "Raster Dataset"),
    ("{4945a015-d612-448d-ae82-24ceb89508ae}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Network Diagram"),
    ("{60ea40cf-2667-45e2-bdff-7f6892538fe8}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Trace Network"),
    ("{70737809-852c-4a03-9e22-2cecea5b9bfa}", "{d4912162-3413-476e-9da4-2aefbbc16939}", "Feature Class"),
    ("{db1b697a-3bb6-426a-98a2-6ee7a4c6aed3}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Toolbox"),
    ("{8c368b12-a12e-4c7e-9638-c9c64e69e98f}", "{8637f1ed-8c04-4866-a44a-1cb8288b3c63}", "Coded Value Domain"),
    ("{73718a66-afb9-4b88-a551-cffa0ae12620}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Geometric Network"),
    ("{dc64b6e4-dc0f-43bd-b4f5-f22385dcf055}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Historical Marker"),
    ("{c673fe0f-7280-404f-8532-20755dd8fc06}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Workspace"),
    ("{fbdd7dd6-4a25-40b7-9a1a-ecc3d1172447}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Tin"),
    ("{76357537-3364-48af-a4be-783c7c28b5cb}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Terrain"),
    ("{767152d3-ed66-4325-8774-420d46674e07}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Topology"),
    ("{cc53cc54-4cca-43b7-9a9b-64dc59c999bd}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Location Referencing Dataset"),
    ("{e6302665-416b-44fa-be33-4e15916ba101}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Survey Dataset"),
    ("{7771fc7d-a38b-4fd3-8225-639d17e9a131}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Network Dataset"),
    ("{dc9ef677-1aa3-45a7-8acd-303a5202d0dc}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Workspace Extension"),
    ("{2d0bfe5d-66c0-4536-8534-b9c50771aae4}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Diagram Dataset"),
    ("{cd06bc3b-789d-4c51-aafa-a467912b8965}", "{d4912162-3413-476e-9da4-2aefbbc16939}", "Table"),
    ("{787bea35-4a86-494f-bb48-500b96145b58}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Catalog Dataset"),
    ("{d86502f9-9758-45c6-9d23-6dd1a0107b47}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Sync Dataset"),
    ("{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "{ffd09c28-fe70-4e25-907c-af8e8a5ec5f3}", "Dataset"),
    ("{d98421eb-d582-4713-9484-43304d0810f6}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Replica Dataset"),
    ("{c29da988-8c3e-45f7-8b5c-18e51ee7beb4}", "{8637f1ed-8c04-4866-a44a-1cb8288b3c63}", "Range Domain"),
    ("{f3783e6f-65ca-4514-8315-ce3985dad3b1}", "{8405add5-8df8-4227-8fac-3fcade073386}", "Folder"),
    ("{37672bd2-b9f3-48c1-89b5-8c43bbbb6d57}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Utility Network"),
    ("{a3803369-5fc2-4963-bae0-13effc09dd73}", "{77292603-930f-475d-ae4f-b8970f42f394}", "Parcel Fabric"),
    ("{4ed4a58e-621f-4043-95ed-850fba45fcbc}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "Replica"),
];

/// The item relationship types as ArcGIS Pro 3.6 writes them (content and order).
#[rustfmt::skip]
const REL_TYPES: &[(&str, &str, &str, &str, &str, &str, i64)] = &[
    ("{ccd6e1c9-9238-40d4-843f-c5dafd3d2bde}", "{37672bd2-b9f3-48c1-89b5-8c43bbbb6d57}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInUtilityNetwork", "Manages Table", "Participates In Utility Network", 0),
    ("{fb166a8c-c09c-4121-9fa7-cb73406e2944}", "{60ea40cf-2667-45e2-bdff-7f6892538fe8}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInTraceNetwork", "Manages Table", "Participates In Trace Network", 0),
    ("{94cd4fc6-307e-430b-a883-5fe68cf19ae8}", "{60ea40cf-2667-45e2-bdff-7f6892538fe8}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInTraceNetwork", "Spatially Manages Feature Class", "Participates In Trace Network", 0),
    ("{583a5baa-3551-41ae-8aa8-1185719f3889}", "{a3803369-5fc2-4963-bae0-13effc09dd73}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInParcelFabric", "Spatially Manages Feature Class", "Participates In Parcel Fabric", 0),
    ("{908a4670-1111-48c6-8269-134fdd3fe617}", "{7771fc7d-a38b-4fd3-8225-639d17e9a131}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInNetworkDataset", "Manages Table", "Participates In Network Dataset", 0),
    ("{d022de33-45bd-424c-88bf-5b1b6b957bd3}", "{d98421eb-d582-4713-9484-43304d0810f6}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "DatasetOfReplicaDataset", "Replicated Dataset", "Dataset of Replicated Dataset", 0),
    ("{0d10b3a7-2f64-45e6-b7ac-2fc27bf2133c}", "{f3783e6f-65ca-4514-8315-ce3985dad3b1}", "{f3783e6f-65ca-4514-8315-ce3985dad3b1}", "FolderInFolder", "Parent Folder Of", "Child Folder Of", 1),
    ("{95c22afd-aa36-431d-b195-1779a256c6b2}", "{37672bd2-b9f3-48c1-89b5-8c43bbbb6d57}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInUtilityNetwork", "Spatially Manages Feature Class", "Participates In Utility Network", 0),
    ("{74dac6c1-a6f2-4603-88a8-d09bccfdcf21}", "{ebeee2c9-fa73-4bed-ac7d-aee7d68afc80}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInParcelDataset", "Spatially Manages Feature Class", "Participates In Parcel Dataset", 0),
    ("{dc739a70-9b71-41e8-868c-008cf46f16d7}", "{73718a66-afb9-4b88-a551-cffa0ae12620}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInGeometricNetwork", "Spatially Manages Feature Class", "Participates In Geometric Network", 0),
    ("{20d1f656-14ba-44cb-9a60-58d2d44f7a83}", "{cc53cc54-4cca-43b7-9a9b-64dc59c999bd}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInLocationReferencingDataset", "Spatially Manages Feature Class", "Participates In Location Referencing Dataset", 0),
    ("{8db31af1-df7c-4632-aa10-3cc44b0c6914}", "{4ed4a58e-621f-4043-95ed-850fba45fcbc}", "{d98421eb-d582-4713-9484-43304d0810f6}", "ReplicaDatasetInReplica", "Replicated Dataset", "Participates In Replica", 1),
    ("{725badab-3452-491b-a795-55f32d67229c}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "DatasetsRelatedThrough", "Origin Of", "Destination Of", 0),
    ("{5f9085e0-788f-4354-ae3c-34c83a7ea784}", "{a3803369-5fc2-4963-bae0-13effc09dd73}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInParcelFabric", "Manages Table", "Participates In Parcel Fabric", 0),
    ("{7d2a7a69-abd8-4aa7-ac08-9e4ddb86289e}", "{ebeee2c9-fa73-4bed-ac7d-aee7d68afc80}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInParcelDataset", "Manages Table", "Participates In Parcel Dataset", 0),
    ("{dc78f1ab-34e4-43ac-ba47-1c4eabd0e7c7}", "{f3783e6f-65ca-4514-8315-ce3985dad3b1}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "DatasetInFolder", "Contains Dataset", "Contained in Dataset", 1),
    ("{55d2f4dc-cb17-4e32-a8c7-47591e8c71de}", "{76357537-3364-48af-a4be-783c7c28b5cb}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInTerrain", "Spatially Manages Feature Class", "Participates In Terrain", 0),
    ("{e79b44e3-f833-4b12-90a1-364ec4ddc43e}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "{a300008d-0cea-4f6a-9dfa-46af829a3df2}", "RepresentationOfFeatureClass", "Feature Class Representation", "Represented Feature Class", 0),
    ("{79cc71c8-b7d9-4141-9014-b6373e236abb}", "{d86502f9-9758-45c6-9d23-6dd1a0107b47}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "DatasetOfSyncDataset", "Replicated Dataset", "Dataset Of Sync Dataset", 0),
    ("{cc28387c-441f-4d7c-a802-41a160317fe0}", "{5b966567-fb87-4dde-938b-b4b37423539d}", "{d86502f9-9758-45c6-9d23-6dd1a0107b47}", "SyncDatasetInSyncReplica", "Sync Dataset", "Participates In Sync Replica", 1),
    ("{d088b110-190b-4229-bdf7-89fddd14d1ea}", "{767152d3-ed66-4325-8774-420d46674e07}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInTopology", "Spatially Manages Feature Class", "Participates In Topology", 0),
    ("{a1633a59-46ba-4448-8706-d8abe2b2b02e}", "{74737149-dcb5-4257-8904-b9724e32a530}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "DatasetInFeatureDataset", "Contains Dataset", "Contained In FeatureDataset", 1),
    ("{b32b8563-0b96-4d32-92c4-086423ae9962}", "{7771fc7d-a38b-4fd3-8225-639d17e9a131}", "{70737809-852c-4a03-9e22-2cecea5b9bfa}", "FeatureClassInNetworkDataset", "Spatially Manages Feature Class", "Participates In Network Dataset", 0),
    ("{5dd0c1af-cb3d-4fea-8c51-cb3ba8d77cdb}", "{f3783e6f-65ca-4514-8315-ce3985dad3b1}", "{8405add5-8df8-4227-8fac-3fcade073386}", "ItemInFolder", "Contains Item", "Contained In Folder", 1),
    ("{17e08adb-2b31-4dcd-8fdd-df529e88f843}", "{28da9e89-ff80-4d6d-8926-4ee2b161677d}", "{8637f1ed-8c04-4866-a44a-1cb8288b3c63}", "DomainInDataset", "Contains Domain", "Contained in Dataset", 0),
    ("{6eedddce-64f9-4549-baad-f05a36c205ed}", "{cc53cc54-4cca-43b7-9a9b-64dc59c999bd}", "{cd06bc3b-789d-4c51-aafa-a467912b8965}", "TableInLocationReferencingDataset", "Manages Table", "Participates In Location Referencing Dataset", 0),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filegdb::{Coord, Geodatabase};

    #[test]
    fn guid_roundtrip_bytes() {
        let b = guid_bytes("{12345678-9ABC-DEF0-1234-567890ABCDEF}").unwrap();
        assert_eq!(b[..4], [0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn write_and_read_back() {
        let dir = std::env::temp_dir().join(format!("terrio_gdb_writer_test_{}.gdb", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = GdbWriter::create(&dir).unwrap();
        w.add_coded_domain("STATUS", "STATUS", FieldType::String, &[("A", "Alpha"), ("B", "Beta")]).unwrap();
        let mut f = FieldDef::new("STATUS", FieldType::String);
        f.length = 50;
        f.alias = "Status".into();
        f.domain = Some("STATUS".into());
        let fields = vec![
            f,
            FieldDef::new("COUNT", FieldType::Int16),
            FieldDef::new("VAL", FieldType::Float64),
            FieldDef::new("POP", FieldType::Float32),
        ];
        w.create_table(&TableDef {
            name: "Areas".into(),
            geometry: Some(GeometryDef::epsg_2278(ShapeType::Polygon)),
            fields: fields.clone(),
        })
        .unwrap();
        w.create_table(&TableDef { name: "Summary".into(), geometry: None, fields }).unwrap();
        let sq = Shape::new(
            ShapeType::Polygon,
            vec![vec![
                Coord::xy(3_000_000.0, 13_800_000.0),
                Coord::xy(3_000_000.0, 13_800_100.0),
                Coord::xy(3_000_100.0, 13_800_100.0),
                Coord::xy(3_000_100.0, 13_800_000.0),
                Coord::xy(3_000_000.0, 13_800_000.0),
            ]],
        );
        for i in 0..1500 {
            w.insert(
                "Areas",
                Some(&sq),
                vec![Value::String("A".into()), Value::Int(i % 100), Value::Float64(i as f64 * 0.5), Value::Float64(2.2)],
            )
            .unwrap();
        }
        w.insert("Areas", None, vec![Value::Null, Value::Null, Value::Null, Value::Null]).unwrap();
        w.insert("Summary", None, vec![Value::String("B".into()), Value::Int(7), Value::Float64(1.25), Value::Null]).unwrap();
        w.finish().unwrap();

        let gdb = Geodatabase::open(&dir).unwrap();
        let mut names = gdb.table_names();
        names.sort();
        assert_eq!(names, ["Areas", "Summary"]);
        let t = gdb.open_table("Areas").unwrap();
        assert_eq!(t.feature_count(), 1501);
        let rows = t.read_all().unwrap();
        assert_eq!(rows.len(), 1501);
        let r = &rows[1200];
        assert_eq!(r.object_id, 1201);
        assert_eq!(r.values[t.field_index("COUNT").unwrap()], Value::Int(0));
        assert_eq!(r.values[t.field_index("POP").unwrap()], Value::Float32(2.2));
        let area = r.values[t.field_index("Shape_Area").unwrap()].as_f64().unwrap();
        assert_eq!(area, r.shape().unwrap().area());
        assert!((area - 10_000.0).abs() < 0.1);
        assert!((r.shape().unwrap().parts[0][1].y - 13_800_100.0).abs() < 1e-3);
        assert!(rows[1500].shape().is_none());
        // ArcGIS stores 0, not null, for the managed fields of a null shape.
        for f in ["Shape_Area", "Shape_Length"] {
            assert_eq!(rows[1500].values[t.field_index(f).unwrap()], Value::Float64(0.0));
        }
        // Spatial index and index registry, as ArcGIS writes them (CheckGeometry
        // reports "could not find spatial index" otherwise).
        assert!(t.has_spatial_index_file());
        let grid = t.spatial_index_grid();
        assert!(grid.len() == 1 && (grid[0] - 100.0).abs() < 1e-3, "{grid:?}");
        let file = |ext: &str| dir.join(format!("a{:08x}.{ext}", 9));
        let spx = std::fs::read(file("spx")).unwrap();
        // 1500 features x a few cells -> depth 2 or 3: a root page, then leaf
        // (and intermediate) pages, then the 22-byte trailer.
        let n_vals = u32::from_le_bytes(spx[spx.len() - 12..spx.len() - 8].try_into().unwrap());
        assert!(n_vals >= 1500);
        let leaves = (n_vals as usize).div_ceil(340);
        let pages = if leaves <= 341 { 1 + leaves } else { 1 + (leaves - 1).div_ceil(340) + leaves };
        assert_eq!(spx.len(), 4096 * pages + 22);
        let idx = std::fs::read(file("gdbindexes")).unwrap();
        assert_eq!(u32::from_le_bytes(idx[..4].try_into().unwrap()), 2);
        let s_tab = gdb.open_table("Summary").unwrap();
        assert!(!s_tab.has_spatial_index_file());
        assert!(dir.join(format!("a{:08x}.gdbindexes", 10)).exists());
        let s = s_tab.read_all().unwrap();
        assert_eq!(s[0].values[1], Value::String("B".into()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_feature_class_gets_empty_spatial_index() {
        let dir = std::env::temp_dir().join(format!("terrio_gdb_writer_empty_{}.gdb", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = GdbWriter::create(&dir).unwrap();
        w.create_table(&TableDef {
            name: "Empty".into(),
            geometry: Some(GeometryDef::epsg_2278(ShapeType::Polygon)),
            fields: vec![FieldDef::new("X", FieldType::Int32)],
        })
        .unwrap();
        w.finish().unwrap();
        let gdb = Geodatabase::open(&dir).unwrap();
        let t = gdb.open_table("Empty").unwrap();
        assert_eq!(t.spatial_index_grid(), &[0.0]);
        assert_eq!(std::fs::read(dir.join("a00000009.spx")).unwrap().len(), 4118);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// ArcGIS Pro rewrites a geodatabase it opens (even read-only) when its item types or item
    /// relationship types are incomplete, or a feature class lacks its `.horizon` file: the
    /// writer produces what ArcGIS Pro 3.6 writes, so opening leaves the files unchanged.
    #[test]
    fn system_tables_and_horizon_as_arcgis_pro_writes_them() {
        let dir = std::env::temp_dir().join(format!("terrio_gdb_writer_horizon_{}.gdb", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = GdbWriter::create(&dir).unwrap();
        w.create_table(&TableDef {
            name: "Fc".into(),
            geometry: Some(GeometryDef::epsg_2278(ShapeType::Polygon)),
            fields: vec![FieldDef::new("X", FieldType::Int32)],
        })
        .unwrap();
        w.create_table(&TableDef { name: "Tab".into(), geometry: None, fields: vec![FieldDef::new("X", FieldType::Int32)] }).unwrap();
        w.finish().unwrap();
        let h = std::fs::read(dir.join("a00000009.horizon")).unwrap();
        let vals: Vec<f64> = h.chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(vals, HORIZON_2278.to_vec());
        assert!(!dir.join("a0000000a.horizon").exists(), "tables get no horizon");
        let gdb = Geodatabase::open(&dir).unwrap();
        let types = gdb.open_table("GDB_ItemTypes").unwrap().read_all().unwrap();
        assert_eq!(types.len(), 39);
        let rels = gdb.open_table("GDB_ItemRelationshipTypes").unwrap().read_all().unwrap();
        assert_eq!(rels.len(), 26);
        let names: Vec<String> = types.iter().filter_map(|r| r.values.last().and_then(|v| v.as_str()).map(String::from)).collect();
        for n in ["Utility Network", "Trace Network", "Sync Replica", "Location Referencing Dataset", "Feature Class", "Coded Value Domain"]
        {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
