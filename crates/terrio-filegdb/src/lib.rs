// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable.cpp, ogropenfilegdblayer.cpp

//! Pure-Rust reader and writer for Esri File Geodatabases (`.gdb` folders).
//!
//! This crate is a port of the relevant parts of GDAL's `OpenFileGDB` driver
//! (<https://github.com/OSGeo/gdal/tree/master/ogr/ogrsf_frmts/openfilegdb>,
//! MIT licence, Copyright (c) 2014-2024 Even Rouault and contributors) and of
//! the format notes at <https://github.com/rouault/dump_gdbtable/wiki/FGDB-Spec>.
//! It has no C dependencies.
//!
//! Reading: [`Geodatabase::open`] then [`Geodatabase::open_table`]. Writing:
//! [`GdbWriter`].

mod geometry;
mod shape;
mod spatial_index;
mod table;
mod varint;
mod writer;

use std::path::{Path, PathBuf};

pub use table::{RowFilter, RowIter, Table};
pub use writer::{ESRI_WKT_2278, FieldDef, GdbWriter, GeometryDef, TableDef};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O error on a file of the geodatabase.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The data does not follow the format.
    #[error("invalid FileGDB data: {0}")]
    Format(String),
    /// A valid but unsupported feature of the format.
    #[error("unsupported FileGDB feature: {0}")]
    Unsupported(String),
    /// No such table.
    #[error("table not found: {0}")]
    NotFound(String),
    /// An invalid argument (writer).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Field types (`FGFT_*` in GDAL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// 16-bit integer (ArcGIS "Short").
    Int16,
    /// 32-bit integer (ArcGIS "Long").
    Int32,
    /// 64-bit integer (ArcGIS Pro 3.2+ "Big Integer").
    Int64,
    /// 32-bit float (ArcGIS "Float").
    Float32,
    /// 64-bit float (ArcGIS "Double").
    Float64,
    /// Text.
    String,
    /// Date and time, days since 1899-12-30.
    DateTime,
    /// The OBJECTID.
    ObjectId,
    /// The shape.
    Geometry,
    /// Binary blob.
    Binary,
    /// Raster.
    Raster,
    /// GUID.
    Guid,
    /// Global ID.
    GlobalId,
    /// XML.
    Xml,
    /// Date only (ArcGIS Pro 3.2+).
    Date,
    /// Time only (ArcGIS Pro 3.2+).
    Time,
    /// Date and time with UTC offset (ArcGIS Pro 3.2+).
    DateTimeOffset,
}

/// Geometry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeType {
    /// Point.
    Point,
    /// Multipoint.
    Multipoint,
    /// Polyline.
    Polyline,
    /// Polygon.
    Polygon,
    /// Multipatch.
    MultiPatch,
}

/// A field description.
#[derive(Debug, Clone)]
pub struct FieldInfo {
    /// Name.
    pub name: String,
    /// Alias.
    pub alias: String,
    /// Type.
    pub field_type: FieldType,
    /// Whether nulls are allowed.
    pub nullable: bool,
    /// Maximum length (text fields; 0 otherwise or unlimited).
    pub length: u32,
}

/// The description of the geometry field of a feature class.
#[derive(Debug, Clone)]
pub struct GeometryFieldInfo {
    /// The declared geometry type.
    pub shape_type: ShapeType,
    /// Whether the feature class has Z values.
    pub has_z: bool,
    /// Whether the feature class has M values.
    pub has_m: bool,
    /// The coordinate system WKT (Esri flavour).
    pub wkt: String,
    /// X origin of the coordinate grid.
    pub x_origin: f64,
    /// Y origin of the coordinate grid.
    pub y_origin: f64,
    /// XY scale (1 / resolution).
    pub xy_scale: f64,
    /// Z origin.
    pub z_origin: f64,
    /// Z scale.
    pub z_scale: f64,
    /// M origin.
    pub m_origin: f64,
    /// M scale.
    pub m_scale: f64,
    /// XY tolerance.
    pub xy_tolerance: f64,
    /// Z tolerance.
    pub z_tolerance: f64,
    /// M tolerance.
    pub m_tolerance: f64,
    /// Extent: xmin, ymin, xmax, ymax.
    pub extent: [f64; 4],
}

/// A row: its OBJECTID and its values, parallel to [`Table::fields`].
#[derive(Debug, Clone)]
pub struct Row {
    /// The OBJECTID.
    pub object_id: i64,
    /// Values in field order (the OBJECTID field holds `Value::Int(oid)`).
    pub values: Vec<Value>,
}

impl Row {
    /// The geometry of the row, if any.
    pub fn shape(&self) -> Option<&Shape> {
        self.values.iter().find_map(|v| match v {
            Value::Geometry(s) => Some(s),
            _ => None,
        })
    }
}

/// A field value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Null.
    Null,
    /// Int16, Int32, Int64 and OBJECTID values (and managed raster ids).
    Int(i64),
    /// Float32 value.
    Float32(f32),
    /// Float64 value.
    Float64(f64),
    /// Text and XML values.
    String(String),
    /// Date/time: days since 1899-12-30 (Date: whole days; Time: fraction of a
    /// day; DateTimeOffset: converted to UTC).
    DateTime(f64),
    /// GUID or GlobalID, as `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
    Guid(String),
    /// Binary value.
    Binary(Vec<u8>),
    /// Geometry.
    Geometry(Shape),
}

impl Value {
    /// The value as f64 (integers and floats), `None` otherwise.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float32(v) => Some(f64::from(*v)),
            Value::Float64(v) | Value::DateTime(v) => Some(*v),
            _ => None,
        }
    }

    /// The value as i64 (integers only), `None` otherwise.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// The value as a string slice (text, XML and GUID values).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::Guid(s) => Some(s),
            _ => None,
        }
    }

    /// Whether the value is null.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// A geometry as stored: parts in file order.
///
/// Polygon parts are rings (Esri orientation: exterior rings clockwise, holes
/// counter-clockwise); polyline parts are paths; a point is one part of one
/// coordinate (no part for an empty point); a multipoint has one part per point.
/// For curved geometries (`has_curves`), the parts hold the vertex chain and
/// `curves` describes which segments are curves. Use [`Shape::area`],
/// [`Shape::length`] and [`Shape::contains_point`] (exact for circular arcs) or
/// [`Shape::densify`] rather than treating the parts as straight segments.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    /// The geometry type.
    pub shape_type: ShapeType,
    /// The parts.
    pub parts: Vec<Vec<Coord>>,
    /// Whether the shape has curve segments (arcs, Beziers, ellipses).
    pub has_curves: bool,
    /// The curve segments (empty for straight geometries).
    pub curves: Vec<CurveSegment>,
}

impl Shape {
    /// A shape of straight segments.
    pub fn new(shape_type: ShapeType, parts: Vec<Vec<Coord>>) -> Self {
        Self { shape_type, parts, has_curves: false, curves: Vec::new() }
    }
}

/// A curve segment of a shape, replacing the straight segment from vertex
/// `start_index` (index into all vertices, parts concatenated) to the next one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CurveSegment {
    /// Circular arc through an interior point (for a full circle, where the
    /// segment starts and ends at the same vertex, the interior point is the
    /// diametrically opposite point and `ccw` gives the direction).
    ArcInteriorPoint {
        /// First vertex of the segment.
        start_index: usize,
        /// Interior point X.
        x: f64,
        /// Interior point Y.
        y: f64,
        /// Counter-clockwise flag of the arc.
        ccw: bool,
    },
    /// Circular arc defined by its center (older encoding).
    ArcCenterPoint {
        /// First vertex of the segment.
        start_index: usize,
        /// Center X.
        center_x: f64,
        /// Center Y.
        center_y: f64,
        /// Counter-clockwise flag.
        ccw: bool,
    },
    /// Cubic Bezier with two control points.
    Bezier {
        /// First vertex of the segment.
        start_index: usize,
        /// First control point X.
        x1: f64,
        /// First control point Y.
        y1: f64,
        /// Second control point X.
        x2: f64,
        /// Second control point Y.
        y2: f64,
    },
    /// Elliptic arc (raw parameters: center x, center y, rotation or
    /// semi-major/minor values, and flag bits, as stored).
    Ellipse {
        /// First vertex of the segment.
        start_index: usize,
        /// The five stored doubles.
        params: [f64; 5],
        /// The stored flag bits.
        bits: u32,
    },
}

impl CurveSegment {
    /// Index of the segment's first vertex.
    pub fn start_index(&self) -> usize {
        match *self {
            CurveSegment::ArcInteriorPoint { start_index, .. }
            | CurveSegment::ArcCenterPoint { start_index, .. }
            | CurveSegment::Bezier { start_index, .. }
            | CurveSegment::Ellipse { start_index, .. } => start_index,
        }
    }
}

/// A coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    /// X.
    pub x: f64,
    /// Y.
    pub y: f64,
    /// Z, for feature classes with Z.
    pub z: Option<f64>,
    /// M, for feature classes with M.
    pub m: Option<f64>,
}

impl Coord {
    /// A 2D coordinate.
    pub fn xy(x: f64, y: f64) -> Self {
        Self { x, y, z: None, m: None }
    }
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    name: String,
    file: PathBuf,
}

/// An open File Geodatabase.
#[derive(Debug)]
pub struct Geodatabase {
    path: PathBuf,
    entries: Vec<CatalogEntry>,
}

impl Geodatabase {
    /// Opens a `.gdb` folder by reading its system catalog (`a00000001.gdbtable`).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = path.join("a00000001.gdbtable");
        if !catalog.exists() {
            return Err(Error::Format(format!("{} is not a File Geodatabase (no a00000001.gdbtable)", path.display())));
        }
        let t = Table::open_file(&catalog, "GDB_SystemCatalog")?;
        let i_name = t.field_index("Name").ok_or_else(|| Error::Format("catalog has no Name field".into()))?;
        let mut entries = Vec::new();
        for row in t.rows()? {
            let row = row?;
            let Some(name) = row.values[i_name].as_str() else {
                continue;
            };
            let file = path.join(format!("a{:08x}.gdbtable", row.object_id));
            if file.exists() {
                entries.push(CatalogEntry { name: name.to_string(), file });
            }
        }
        Ok(Self { path, entries })
    }

    /// The path of the geodatabase.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// User tables and feature classes (system `GDB_*` tables excluded).
    pub fn table_names(&self) -> Vec<String> {
        self.entries.iter().filter(|e| !e.name.to_ascii_uppercase().starts_with("GDB_")).map(|e| e.name.clone()).collect()
    }

    /// All catalog entries, system tables included.
    pub fn all_table_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// Whether a table exists (case-insensitive).
    pub fn has_table(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// Opens a table by name (case-insensitive).
    pub fn open_table(&self, name: &str) -> Result<Table> {
        let e = self
            .entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::NotFound(format!("{name} in {}", self.path.display())))?;
        Table::open_file(&e.file, &e.name)
    }
}
