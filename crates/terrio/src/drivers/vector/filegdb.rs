//! The `OpenFileGDB` vector driver: Esri File Geodatabases (`.gdb` directories), read only, through
//! the pure-Rust `crate::filegdb` module.
//!
//! Every user table is a layer (the system `GDB_*` tables are not), empty ones and those without
//! geometry included. As in GDAL's driver, OBJECTID is the feature id and no field; `Shape_Length`
//! and `Shape_Area` are ordinary fields; polyline features are always multilinestrings and polygon
//! features multipolygons, the rings organized by orientation (`ONLY_CCW`).
//!
//! What terrio's vector model cannot hold is lost: see [`ARC_STEP_DEGREES`] for curves,
//! [`value`] for date and time values and [`read_table`] for raster fields and multipatches.

// Written to match the layers of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/openfilegdb/ogropenfilegdblayer.cpp
// No code is translated from this source: the format's code is in `crate::filegdb`.

use super::datetime::{format_date, format_datetime};
use crate::dataset::{VectorOpenOptions, fallible};
use crate::filegdb::{FieldInfo, FieldType as GdbType, Geodatabase, RowFilter, Shape, ShapeType, Table, Value};
use crate::formats::{Kind, VectorDriver};
use crate::{Error, Result};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// The largest angle, in degrees, a straight segment replaces along a true curve: circular arcs
/// are densified at this step, the exact curve parameters being lost (terrio's vector model has
/// straight segments only). A full circle gets about 360 vertices, on the circle, and an area 0.005%
/// too small, where GDAL's default step of 4 degrees gives 90 vertices and 0.08%.
pub const ARC_STEP_DEGREES: f64 = 1.0;

/// How often, in retained bytes, the features read are checked against usable memory.
const CHECK_EVERY: usize = 64 << 20;

/// Days from the epoch of File Geodatabase dates (1899-12-30) to the Unix epoch.
const DAYS_TO_UNIX_EPOCH: f64 = 25_569.0;

/// Dates further from the epoch than this many days (about 2.7 million years) are corrupt.
const MAX_DATE_DAYS: f64 = 1e9;

/// The memory the features read so far hold, against what the process can use (as other whole-file
/// readers check it): a refusal, not a layer left out, when they exceed it.
struct Retained {
    bytes: usize,
    next_check: usize,
    check: fn(usize) -> std::result::Result<usize, String>,
}

impl Retained {
    fn new(check: fn(usize) -> std::result::Result<usize, String>) -> Self {
        Self { bytes: 0, next_check: 0, check }
    }

    /// Adds `bytes` of features, while a table's `transient` bytes are in memory too.
    fn charge(&mut self, bytes: usize, transient: usize) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes >= self.next_check {
            (self.check)(self.bytes.saturating_add(transient)).map_err(Error::ResourceLimit)?;
            self.next_check = self.bytes.saturating_add(CHECK_EVERY);
        }
        Ok(())
    }
}

fn usable(bytes: usize) -> std::result::Result<usize, String> {
    fallible::check_usable(Some(bytes), || "OpenFileGDB: the layers are too large to hold in memory".to_string())
}

fn error(e: crate::filegdb::Error) -> Error {
    match e {
        crate::filegdb::Error::Io { path, source } => {
            Error::Io(std::io::Error::new(source.kind(), format!("{}: {source}", path.display())))
        }
        crate::filegdb::Error::Format(m) => Error::InvalidData(format!("OpenFileGDB: {m}")),
        crate::filegdb::Error::Unsupported(m) => Error::Unsupported(format!("OpenFileGDB: {m}")),
        e @ (crate::filegdb::Error::NotFound(_) | crate::filegdb::Error::InvalidArgument(_)) => Error::InvalidArgument(e.to_string()),
    }
}

fn open(path: &str) -> Result<Geodatabase> {
    crate::formats::check_driver_allowed(VectorDriver::FileGDB.name(), path, Kind::Vector)?;
    Geodatabase::open(path).map_err(error)
}

/// The layer names of a geodatabase, from its catalog.
pub(crate) fn list_layers(path: &str) -> Result<Vec<String>> {
    Ok(open(path)?.table_names())
}

/// Reads the layers `options` selects, keeping only their range of features and, with a spatial
/// filter, the features whose envelope meets it (no row with a null shape).
pub(crate) fn read(path: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    read_within(path, options, usable)
}

fn read_within(path: &str, options: &VectorOpenOptions, check: fn(usize) -> std::result::Result<usize, String>) -> Result<Vec<Layer>> {
    let gdb = open(path)?;
    let names = gdb.table_names();
    let targeted = !options.layers.is_empty();
    // A lone table is read whatever the name asked for: the dataset's name may also select it.
    let wanted: Vec<&String> = names.iter().filter(|n| !targeted || names.len() == 1 || options.layers.contains(n)).collect();
    if targeted && wanted.is_empty() {
        return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), names.join(", "))));
    }
    let mut retained = Retained::new(check);
    let mut layers = Vec::with_capacity(wanted.len());
    for name in &wanted {
        match read_table(&gdb, name, options, &mut retained) {
            Ok(layer) => layers.push(layer),
            // One table of a kind this reader doesn't know should not hide the others.
            Err(e @ Error::Unsupported(_)) if !targeted => {
                crate::warnings::warn(format!("OpenFileGDB: layer {name} left out: {e} (name it with a layer option to see it fail)"));
            }
            Err(e) => return Err(e),
        }
    }
    if layers.is_empty() && !wanted.is_empty() {
        return Err(Error::Unsupported(format!("'{path}': every table of the geodatabase was left out (see the warnings)")));
    }
    Ok(layers)
}

/// Reads a table as a layer. Raster fields are left out, with a warning (their values have a form
/// per field the crate doesn't report). Multipatch shapes keep their attributes but have no
/// geometry, also with a warning: the crate doesn't keep their part types (triangle strips, fans,
/// rings), so that their vertices don't tell their surfaces.
fn read_table(gdb: &Geodatabase, name: &str, options: &VectorOpenOptions, retained: &mut Retained) -> Result<Layer> {
    let table = gdb.open_table(name).map_err(error)?;
    let mut layer = Layer::new(table.name());
    let columns = schema(&table, &mut layer);
    if let Some(info) = table.geometry_info() {
        layer.geom_type = match info.shape_type {
            ShapeType::Point => Some(GeometryType::Point),
            ShapeType::Multipoint => Some(GeometryType::MultiPoint),
            ShapeType::Polyline => Some(GeometryType::MultiLineString),
            ShapeType::Polygon => Some(GeometryType::MultiPolygon),
            ShapeType::MultiPatch => {
                crate::warnings::warn(format!("OpenFileGDB: layer {name}: multipatch geometries are not read, only their attributes"));
                None
            }
        };
        let wkt = info.wkt.trim();
        if !wkt.is_empty() && !wkt.eq_ignore_ascii_case("unknown") {
            layer.set_crs_wkt(Some(wkt.to_string()));
        }
    }
    // The whole table file is in memory while its rows are read, and each row takes at least 4 bytes.
    let file_bytes = usize::try_from(std::fs::metadata(table.path())?.len()).unwrap_or(usize::MAX);
    retained.charge(0, file_bytes)?;
    let max = options.max_features.unwrap_or(usize::MAX);
    let declared = usize::try_from(table.feature_count()).unwrap_or(usize::MAX).min(file_bytes / 4);
    fallible::reserve(&mut layer.features, declared.saturating_sub(options.skip_features).min(max), "the features of a layer")?;
    let filter = RowFilter { columns: None, bbox: options.spatial_filter.map(|(x0, y0, x1, y1)| [x0, y0, x1, y1]) };
    let mut rows = table.rows_filtered(filter).map_err(error)?;
    let mut skip = options.skip_features;
    while layer.features.len() < max {
        let Some(row) = rows.next() else { break };
        let mut row = row.map_err(error)?;
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let geometry = row.shape().and_then(to_geometry);
        let attributes: Vec<FieldValue> =
            columns.iter().map(|&(i, field_type)| value(field_type, std::mem::replace(&mut row.values[i], Value::Null))).collect();
        let bytes = std::mem::size_of::<Feature>()
            + attributes.len() * std::mem::size_of::<FieldValue>()
            + attributes.iter().map(heap_bytes).sum::<usize>()
            + geometry.as_ref().map_or(0, |g| vertices(g) * std::mem::size_of::<Coord>());
        retained.charge(bytes, file_bytes)?;
        layer.features.push(Feature { fid: row.object_id as u64, geometry, attributes });
    }
    Ok(layer)
}

/// Adds the table's fields to the layer's schema, and gives their columns and types.
fn schema(table: &Table, layer: &mut Layer) -> Vec<(usize, GdbType)> {
    let mut columns = Vec::with_capacity(table.fields().len());
    for (i, field) in table.fields().iter().enumerate() {
        if field.field_type == GdbType::Raster {
            crate::warnings::warn(format!("OpenFileGDB: layer {}: raster field {} left out", table.name(), field.name));
        }
        if let Some(def) = field_def(field) {
            layer.add_field(def);
            columns.push((i, field.field_type));
        }
    }
    columns
}

/// The field of a table column: none for the OBJECTID (the feature id), the shape (the geometry)
/// and raster columns.
fn field_def(field: &FieldInfo) -> Option<FieldDef> {
    let field_type = match field.field_type {
        GdbType::ObjectId | GdbType::Geometry | GdbType::Raster => return None,
        GdbType::Int16 | GdbType::Int32 | GdbType::Int64 => FieldType::Integer,
        GdbType::Float32 | GdbType::Float64 => FieldType::Float,
        GdbType::String | GdbType::Guid | GdbType::GlobalId | GdbType::Xml | GdbType::Time => FieldType::Text,
        GdbType::Date => FieldType::Date,
        GdbType::DateTime | GdbType::DateTimeOffset => FieldType::DateTime,
        GdbType::Binary => FieldType::Blob,
    };
    let mut def = FieldDef::new(field.name.clone(), field_type);
    if field.field_type == GdbType::String {
        def = def.width(field.length as usize);
    }
    Some(if field.nullable { def } else { def.not_null() })
}

/// A value in terrio's vector model: dates as text to the millisecond, times of day (which the model
/// has no type for) as `HH:MM:SS` text, and date-times with a UTC offset as UTC, marked `Z`, their
/// offset lost (the crate reads them so). Non-finite and absurd dates are null.
fn value(field_type: GdbType, value: Value) -> FieldValue {
    match value {
        Value::Int(v) => FieldValue::Integer(v),
        Value::Float32(v) => FieldValue::Float(f64::from(v)),
        Value::Float64(v) => FieldValue::Float(v),
        Value::String(s) | Value::Guid(s) => FieldValue::Text(s),
        Value::Binary(b) => FieldValue::Blob(b),
        Value::DateTime(days) if !days.is_finite() || days.abs() > MAX_DATE_DAYS => FieldValue::Null,
        Value::DateTime(days) => match field_type {
            GdbType::Date => FieldValue::Date(format_date((days.floor() - DAYS_TO_UNIX_EPOCH) as i64)),
            GdbType::Time => {
                let ms = (days.rem_euclid(1.0) * 86_400_000.0).round() as i64 % 86_400_000;
                FieldValue::Text(format_datetime(ms, false).split_off("1970-01-01T".len()))
            }
            _ => {
                let ms = ((days - DAYS_TO_UNIX_EPOCH) * 86_400_000.0).round() as i64;
                FieldValue::DateTime(format_datetime(ms, field_type == GdbType::DateTimeOffset))
            }
        },
        Value::Null | Value::Geometry(_) => FieldValue::Null,
    }
}

fn heap_bytes(value: &FieldValue) -> usize {
    match value {
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.len(),
        FieldValue::Blob(b) => b.len(),
        _ => 0,
    }
}

fn vertices(geometry: &Geometry) -> usize {
    let polygon =
        |exterior: &wbvector::Ring, interiors: &[wbvector::Ring]| exterior.len() + interiors.iter().map(|r| r.len()).sum::<usize>();
    match geometry {
        Geometry::Point(_) => 1,
        Geometry::LineString(c) | Geometry::MultiPoint(c) => c.len(),
        Geometry::Polygon { exterior, interiors } => polygon(exterior, interiors),
        Geometry::MultiLineString(lines) => lines.iter().map(Vec::len).sum(),
        Geometry::MultiPolygon(polygons) => polygons.iter().map(|(e, i)| polygon(e, i)).sum(),
        Geometry::GeometryCollection(geometries) => geometries.iter().map(vertices).sum(),
    }
}

/// A shape as a geometry, its curves densified; `None` for an empty shape or a multipatch.
fn to_geometry(shape: &Shape) -> Option<Geometry> {
    let densified;
    let shape = if shape.has_curves {
        densified = shape.densify(ARC_STEP_DEGREES);
        &densified
    } else {
        shape
    };
    let coord = |c: &crate::filegdb::Coord| Coord { x: c.x, y: c.y, z: c.z, m: c.m.filter(|m| !m.is_nan()) };
    let mut parts = shape.parts.iter().filter(|p| !p.is_empty()).map(|p| p.iter().map(coord).collect::<Vec<Coord>>()).peekable();
    parts.peek()?;
    match shape.shape_type {
        ShapeType::Point => parts.next()?.into_iter().next().map(Geometry::Point),
        ShapeType::Multipoint => Some(Geometry::MultiPoint(parts.flatten().collect())),
        ShapeType::Polyline => Some(Geometry::MultiLineString(parts.collect())),
        ShapeType::Polygon => {
            let rings: Vec<Vec<Coord>> = parts.filter(|r| r.len() >= 3).collect();
            if rings.is_empty() {
                return None;
            }
            Some(match super::shapefile::organize_rings(rings) {
                Geometry::Polygon { exterior, interiors } => Geometry::MultiPolygon(vec![(exterior, interiors)]),
                multi => multi,
            })
        }
        ShapeType::MultiPatch => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filegdb::{Coord as C, CurveSegment, FieldDef as F, GdbWriter, GeometryDef, TableDef};
    use crate::{Dataset, VectorDataset};
    use std::collections::HashMap;

    /// 2021-03-04 05:06:07 in days since 1899-12-30.
    const DATETIME: f64 = 44_259.0 + (5.0 * 3600.0 + 6.0 * 60.0 + 7.0) / 86_400.0;
    const GUID: &str = "{01234567-89AB-CDEF-0123-456789ABCDEF}";

    fn field(name: &str, field_type: GdbType) -> F {
        F { length: if field_type == GdbType::String { 20 } else { 0 }, ..F::new(name, field_type) }
    }

    fn all_fields() -> Vec<F> {
        vec![
            field("Small", GdbType::Int16),
            field("Medium", GdbType::Int32),
            field("Large", GdbType::Int64),
            field("Single", GdbType::Float32),
            field("Double", GdbType::Float64),
            field("Label", GdbType::String),
            field("Stamp", GdbType::DateTime),
            field("Day", GdbType::Date),
            field("Clock", GdbType::Time),
            field("Offset", GdbType::DateTimeOffset),
            field("Ref", GdbType::Guid),
            field("Global", GdbType::GlobalId),
            field("Raw", GdbType::Binary),
            field("Doc", GdbType::Xml),
        ]
    }

    fn square(x: f64, y: f64, side: f64, clockwise: bool) -> Vec<C> {
        let mut ring = vec![C::xy(x, y), C::xy(x, y + side), C::xy(x + side, y + side), C::xy(x + side, y), C::xy(x, y)];
        if !clockwise {
            ring.reverse();
        }
        ring
    }

    /// A circle stored as a true curve: one arc from a vertex back to itself.
    fn circle(x: f64, y: f64, r: f64) -> Shape {
        let p = C::xy(x, y + r);
        Shape {
            shape_type: ShapeType::Polygon,
            parts: vec![vec![p, p]],
            has_curves: true,
            curves: vec![CurveSegment::ArcInteriorPoint { start_index: 0, x, y: y - r, ccw: false }],
        }
    }

    /// A geodatabase of six tables: `Areas` (every field type, a null shape, a circle), `Lines`,
    /// `Points`, `Clusters`, the empty `Nothing` and the non-spatial `Lookup`.
    fn sample(dir: &tempfile::TempDir) -> String {
        let path = dir.path().join("sample.gdb");
        let mut w = GdbWriter::create(&path).unwrap();
        let spatial =
            |name: &str, shape_type, fields| TableDef { name: name.into(), geometry: Some(GeometryDef::epsg_2278(shape_type)), fields };
        w.create_table(&spatial("Areas", ShapeType::Polygon, all_fields())).unwrap();
        w.create_table(&spatial("Lines", ShapeType::Polyline, vec![field("Label", GdbType::String)])).unwrap();
        w.create_table(&spatial("Points", ShapeType::Point, vec![])).unwrap();
        w.create_table(&spatial("Clusters", ShapeType::Multipoint, vec![])).unwrap();
        w.create_table(&spatial("Nothing", ShapeType::Polygon, vec![field("Label", GdbType::String)])).unwrap();
        w.create_table(&TableDef {
            name: "Lookup".into(),
            geometry: None,
            fields: vec![field("Code", GdbType::Int32), field("Label", GdbType::String)],
        })
        .unwrap();

        let holed = Shape::new(ShapeType::Polygon, vec![square(1000.0, 2000.0, 100.0, true), square(1010.0, 2010.0, 10.0, false)]);
        let values = vec![
            Value::Int(-7),
            Value::Int(70_000),
            Value::Int(5_000_000_000),
            Value::Float32(1.5),
            Value::Float64(2.25),
            Value::String("first".into()),
            Value::DateTime(DATETIME),
            Value::DateTime(44_259.0),
            Value::DateTime(0.25),
            Value::Null, // The writer stores no date-time with an offset: see `value_forms`.
            Value::Guid(GUID.into()),
            Value::Guid(GUID.into()),
            Value::Binary(vec![1, 2, 255]),
            Value::String("<a>b</a>".into()),
        ];
        let nulls = vec![Value::Null; values.len()];
        w.insert("Areas", Some(&holed), values).unwrap();
        w.insert("Areas", None, nulls.clone()).unwrap();
        w.insert("Areas", Some(&circle(5000.0, 6000.0, 50.0)), nulls).unwrap();

        let line =
            Shape::new(ShapeType::Polyline, vec![vec![C::xy(0.0, 0.0), C::xy(10.0, 0.0)], vec![C::xy(20.0, 20.0), C::xy(30.0, 25.0)]]);
        w.insert("Lines", Some(&line), vec![Value::String("two parts".into())]).unwrap();
        w.insert("Points", Some(&Shape::new(ShapeType::Point, vec![vec![C::xy(12.5, 13.5)]])), vec![]).unwrap();
        let cluster = Shape::new(ShapeType::Multipoint, vec![vec![C::xy(1.0, 2.0)], vec![C::xy(3.0, 4.0)]]);
        w.insert("Clusters", Some(&cluster), vec![]).unwrap();
        w.insert("Lookup", None, vec![Value::Int(1), Value::String("one".into())]).unwrap();
        w.insert("Lookup", None, vec![Value::Int(2), Value::Null]).unwrap();
        w.finish().unwrap();
        path.to_str().unwrap().to_string()
    }

    fn opened(path: &str, options: &VectorOpenOptions) -> VectorDataset {
        crate::open_vector(path, options).unwrap()
    }

    fn layer<'a>(ds: &'a VectorDataset, name: &str) -> &'a Layer {
        ds.layers.iter().find(|l| l.name == name).unwrap_or_else(|| panic!("no layer {name}"))
    }

    fn attr<'a>(l: &'a Layer, feature: usize, field: &str) -> &'a FieldValue {
        &l.features[feature].attributes[l.schema.field_index(field).unwrap_or_else(|| panic!("no field {field}"))]
    }

    /// Half a step of the coordinate grid of the sample's CRS (1 / 3048.006 ft), to which the writer
    /// rounds coordinates.
    const GRID: f64 = 1.65e-4;

    /// Asserts the coordinates are those expected, as the writer rounded them to its grid.
    fn assert_xy(cs: &[Coord], expected: &[(f64, f64)]) {
        assert_eq!(cs.len(), expected.len(), "{cs:?}");
        for (c, e) in cs.iter().zip(expected) {
            assert!((c.x - e.0).abs() <= GRID && (c.y - e.1).abs() <= GRID, "({}, {}) for {e:?}", c.x, c.y);
        }
    }

    fn float(value: &FieldValue) -> f64 {
        let FieldValue::Float(v) = value else { panic!("{value:?}") };
        *v
    }

    /// `f` under `TERRIO_ALLOWED_DRIVERS=drivers`.
    fn under<T>(drivers: &str, f: impl FnOnce() -> T) -> T {
        crate::config::push_scope(HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some(drivers.to_string()))]));
        let out = f();
        crate::config::pop_scope();
        out
    }

    #[test]
    fn open_recognizes_the_directory_and_gives_every_table_as_a_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let Dataset::Vector(ds) = crate::open(&path).unwrap() else { panic!("a raster") };
        assert_eq!(ds.driver, VectorDriver::FileGDB);
        let names: Vec<&str> = ds.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Areas", "Lines", "Points", "Clusters", "Nothing", "Lookup"]);
        // A trailing separator names the same directory.
        let Dataset::Vector(ds) = crate::open(&format!("{path}/")).unwrap() else { panic!("a raster") };
        assert_eq!(ds.layers.len(), 6);
    }

    #[test]
    fn every_field_type_keeps_its_name_type_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let l = layer(&ds, "Areas");
        let fields: Vec<(&str, FieldType)> = l.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect();
        assert_eq!(
            fields,
            [
                ("Small", FieldType::Integer),
                ("Medium", FieldType::Integer),
                ("Large", FieldType::Integer),
                ("Single", FieldType::Float),
                ("Double", FieldType::Float),
                ("Label", FieldType::Text),
                ("Stamp", FieldType::DateTime),
                ("Day", FieldType::Date),
                ("Clock", FieldType::Text),
                ("Offset", FieldType::DateTime),
                ("Ref", FieldType::Text),
                ("Global", FieldType::Text),
                ("Raw", FieldType::Blob),
                ("Doc", FieldType::Text),
                ("Shape_Length", FieldType::Float),
                ("Shape_Area", FieldType::Float),
            ]
        );
        assert_eq!(l.schema.field("Label").unwrap().width, 20);
        assert_eq!(attr(l, 0, "Small"), &FieldValue::Integer(-7));
        assert_eq!(attr(l, 0, "Medium"), &FieldValue::Integer(70_000));
        assert_eq!(attr(l, 0, "Large"), &FieldValue::Integer(5_000_000_000));
        assert_eq!(attr(l, 0, "Single"), &FieldValue::Float(1.5));
        assert_eq!(attr(l, 0, "Double"), &FieldValue::Float(2.25));
        assert_eq!(attr(l, 0, "Label"), &FieldValue::Text("first".into()));
        assert_eq!(attr(l, 0, "Stamp"), &FieldValue::DateTime("2021-03-04T05:06:07".into()));
        assert_eq!(attr(l, 0, "Day"), &FieldValue::Date("2021-03-04".into()));
        assert_eq!(attr(l, 0, "Clock"), &FieldValue::Text("06:00:00".into()));
        assert_eq!(attr(l, 0, "Offset"), &FieldValue::Null);
        assert_eq!(attr(l, 0, "Ref"), &FieldValue::Text(GUID.into()));
        assert_eq!(attr(l, 0, "Global"), &FieldValue::Text(GUID.into()));
        assert_eq!(attr(l, 0, "Raw"), &FieldValue::Blob(vec![1, 2, 255]));
        assert_eq!(attr(l, 0, "Doc"), &FieldValue::Text("<a>b</a>".into()));
        // The hole counts against the area: 100^2 - 10^2. The perimeters of both rings count. Both
        // are computed from the coordinates rounded to the grid.
        assert!((float(attr(l, 0, "Shape_Area")) - 9900.0).abs() < 0.1, "{:?}", attr(l, 0, "Shape_Area"));
        assert!((float(attr(l, 0, "Shape_Length")) - 440.0).abs() < 0.01, "{:?}", attr(l, 0, "Shape_Length"));
    }

    /// Date-times with an offset, which the writer doesn't store, and dates a file can corrupt.
    #[test]
    fn value_forms() {
        assert_eq!(value(GdbType::DateTimeOffset, Value::DateTime(DATETIME)), FieldValue::DateTime("2021-03-04T05:06:07Z".into()));
        assert_eq!(
            value(GdbType::DateTime, Value::DateTime(DATETIME + 0.5 / 86_400.0)),
            FieldValue::DateTime("2021-03-04T05:06:07.500".into())
        );
        // Before 1970, and before the epoch of the format.
        assert_eq!(value(GdbType::Date, Value::DateTime(1.75)), FieldValue::Date("1899-12-31".into()));
        assert_eq!(value(GdbType::Date, Value::DateTime(-1.0)), FieldValue::Date("1899-12-29".into()));
        // A time that rounds up to midnight stays within the day.
        assert_eq!(value(GdbType::Time, Value::DateTime(0.999_999_999_9)), FieldValue::Text("00:00:00".into()));
        for bad in [f64::NAN, f64::INFINITY, f64::MAX, -f64::MAX] {
            for field_type in [GdbType::Date, GdbType::Time, GdbType::DateTime] {
                assert_eq!(value(field_type, Value::DateTime(bad)), FieldValue::Null, "{bad} {field_type:?}");
            }
        }
    }

    #[test]
    fn null_shapes_stay_features_without_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let l = layer(&ds, "Areas");
        assert_eq!(l.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(l.features[1].geometry.is_none());
        assert_eq!(attr(l, 1, "Label"), &FieldValue::Null);
        assert_eq!(attr(l, 1, "Stamp"), &FieldValue::Null);
        assert_eq!(attr(l, 1, "Raw"), &FieldValue::Null);
        // Managed by the geodatabase, as ArcGIS stores them.
        assert_eq!(attr(l, 1, "Shape_Area"), &FieldValue::Float(0.0));
    }

    #[test]
    fn empty_and_non_spatial_tables_are_layers() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let empty = layer(&ds, "Nothing");
        assert_eq!(empty.features.len(), 0);
        assert_eq!(empty.geom_type, Some(GeometryType::MultiPolygon));
        assert_eq!(empty.crs_epsg(), Some(2278));
        let fields: Vec<&str> = empty.schema.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, ["Label", "Shape_Length", "Shape_Area"]);

        let lookup = layer(&ds, "Lookup");
        assert_eq!(lookup.geom_type, None);
        assert_eq!(lookup.crs_epsg(), None);
        assert_eq!(lookup.crs_wkt(), None);
        assert_eq!(lookup.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2]);
        assert!(lookup.features.iter().all(|f| f.geometry.is_none()));
        assert_eq!(attr(lookup, 0, "Code"), &FieldValue::Integer(1));
        assert_eq!(attr(lookup, 0, "Label"), &FieldValue::Text("one".into()));
        assert_eq!(attr(lookup, 1, "Code"), &FieldValue::Integer(2));
        assert_eq!(attr(lookup, 1, "Label"), &FieldValue::Null);
    }

    #[test]
    fn geometries_and_the_crs_come_through() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let areas = layer(&ds, "Areas");
        assert_eq!(areas.geom_type, Some(GeometryType::MultiPolygon));
        assert_eq!(areas.crs_epsg(), Some(2278));
        assert_eq!(areas.crs_wkt(), Some(crate::filegdb::ESRI_WKT_2278));
        let Some(Geometry::MultiPolygon(polygons)) = &areas.features[0].geometry else { panic!("{:?}", areas.features[0].geometry) };
        let [(exterior, interiors)] = polygons.as_slice() else { panic!("{polygons:?}") };
        assert_xy(exterior.coords(), &[(1000.0, 2000.0), (1000.0, 2100.0), (1100.0, 2100.0), (1100.0, 2000.0)]);
        let [hole] = interiors.as_slice() else { panic!("{interiors:?}") };
        assert_xy(hole.coords(), &[(1010.0, 2010.0), (1020.0, 2010.0), (1020.0, 2020.0), (1010.0, 2020.0)]);

        let lines = layer(&ds, "Lines");
        assert_eq!(lines.geom_type, Some(GeometryType::MultiLineString));
        let Some(Geometry::MultiLineString(parts)) = &lines.features[0].geometry else { panic!("{:?}", lines.features[0].geometry) };
        assert_xy(&parts[0], &[(0.0, 0.0), (10.0, 0.0)]);
        assert_xy(&parts[1], &[(20.0, 20.0), (30.0, 25.0)]);

        let points = layer(&ds, "Points");
        assert_eq!(points.geom_type, Some(GeometryType::Point));
        let Some(Geometry::Point(point)) = &points.features[0].geometry else { panic!("{:?}", points.features[0].geometry) };
        assert_xy(std::slice::from_ref(point), &[(12.5, 13.5)]);
        assert_eq!((point.z, point.m), (None, None));

        let clusters = layer(&ds, "Clusters");
        assert_eq!(clusters.geom_type, Some(GeometryType::MultiPoint));
        let Some(Geometry::MultiPoint(members)) = &clusters.features[0].geometry else { panic!("{:?}", clusters.features[0].geometry) };
        assert_xy(members, &[(1.0, 2.0), (3.0, 4.0)]);
    }

    #[test]
    fn circles_are_densified_at_the_documented_step() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let areas = layer(&ds, "Areas");
        let Some(Geometry::MultiPolygon(polygons)) = &areas.features[2].geometry else { panic!("{:?}", areas.features[2].geometry) };
        let [(ring, interiors)] = polygons.as_slice() else { panic!("{polygons:?}") };
        assert!(interiors.is_empty());
        assert!((360..=361).contains(&ring.len()), "{} vertices", ring.len());
        // On the circle, to the coordinate grid of about 0.0003 ft and its rounding.
        for c in ring.coords() {
            let radius = (c.x - 5000.0).hypot(c.y - 6000.0);
            assert!((radius - 50.0).abs() < 1e-3, "({}, {}) at {radius}", c.x, c.y);
        }
        // The chord polygon is smaller than the circle by the documented 0.005% (5.08e-5, within what
        // rounding the radius to the grid changes), while the area stored in the table, 0.4 larger,
        // stays the circle's.
        let area = ring.signed_area().abs();
        let exact = std::f64::consts::PI * 2500.0;
        assert!((1.0 - area / exact - 5.08e-5).abs() < 1e-5, "{area} of {exact}");
        let stored = float(attr(areas, 2, "Shape_Area"));
        assert!((stored - exact).abs() < 0.1, "{stored} of {exact}");
    }

    #[test]
    fn layer_options_select_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let options = VectorOpenOptions { layers: vec!["Lookup".into(), "Points".into()], ..Default::default() };
        let names: Vec<String> = opened(&path, &options).layers.into_iter().map(|l| l.name).collect();
        assert_eq!(names, ["Points", "Lookup"]);
        let options = VectorOpenOptions { layers: vec!["Missing".into()], ..Default::default() };
        let e = crate::open_vector(&path, &options).unwrap_err().to_string();
        assert!(e.contains("no layer named Missing") && e.contains("Lookup"), "{e}");
        assert_eq!(crate::list_layers(&path).unwrap(), ["Areas", "Lines", "Points", "Clusters", "Nothing", "Lookup"]);
    }

    #[test]
    fn feature_ranges_keep_object_ids_and_count_after_the_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let fids = |options: &VectorOpenOptions| layer(&opened(&path, options), "Areas").features.iter().map(|f| f.fid).collect::<Vec<_>>();
        assert_eq!(fids(&VectorOpenOptions { skip_features: 1, ..Default::default() }), [2, 3]);
        assert_eq!(fids(&VectorOpenOptions { max_features: Some(1), ..Default::default() }), [1]);
        assert_eq!(fids(&VectorOpenOptions { skip_features: 1, max_features: Some(1), ..Default::default() }), [2]);
        assert_eq!(fids(&VectorOpenOptions { skip_features: 9, ..Default::default() }), Vec::<u64>::new());
        assert_eq!(fids(&VectorOpenOptions { max_features: Some(0), ..Default::default() }), Vec::<u64>::new());
    }

    #[test]
    fn spatial_filters_skip_by_envelope_but_not_tables_without_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let around_circle = VectorOpenOptions { spatial_filter: Some((4900.0, 5900.0, 5100.0, 6100.0)), ..Default::default() };
        let ds = opened(&path, &around_circle);
        // Neither the far polygon nor the null shape.
        assert_eq!(layer(&ds, "Areas").features.iter().map(|f| f.fid).collect::<Vec<_>>(), [3]);
        assert_eq!(layer(&ds, "Lines").features.len(), 0);
        assert_eq!(layer(&ds, "Lookup").features.len(), 2);
    }

    #[test]
    fn the_allow_list_judges_the_driver_by_its_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let refused = format!("'{path}': the OPENFILEGDB driver is not allowed (allowed: GEOJSON)");
        assert_eq!(under("GeoJSON", || crate::open(&path).map(|_| ())).unwrap_err().to_string(), refused);
        assert_eq!(under("GeoJSON", || crate::open_vector(&path, &Default::default()).map(|_| ())).unwrap_err().to_string(), refused);
        assert_eq!(under("GeoJSON", || crate::list_layers(&path).map(|_| ())).unwrap_err().to_string(), refused);
        // The reader judges on its own, for what reaches it by another way.
        let holds = format!("'{path}' holds OpenFileGDB data, and the OpenFileGDB driver is not allowed (allowed: GEOJSON)");
        assert_eq!(under("GeoJSON", || read(&path, &Default::default()).map(|_| ())).unwrap_err().to_string(), holds);
        assert_eq!(under("GeoJSON", || list_layers(&path).map(|_| ())).unwrap_err().to_string(), holds);
        for name in ["OpenFileGDB", "FileGDB"] {
            assert_eq!(under(name, || crate::open_vector(&path, &Default::default()).map(|d| d.layers.len())).unwrap(), 6, "{name}");
        }
    }

    #[test]
    fn a_refusal_of_memory_stops_the_read_rather_than_leaving_layers_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let refused = read_within(&path, &Default::default(), |_| Err("too large".to_string())).unwrap_err();
        assert!(matches!(&refused, Error::ResourceLimit(m) if m == "too large"), "{refused}");
        assert!(refused.is_resource_refusal());
        assert_eq!(read_within(&path, &Default::default(), Ok).unwrap().len(), 6);
    }

    #[test]
    fn the_driver_is_listed_read_only_with_gdals_name() {
        assert_eq!(VectorDriver::for_extension("a/b.gdb"), Some(VectorDriver::FileGDB));
        assert_eq!(VectorDriver::for_extension("a/B.GDB/"), Some(VectorDriver::FileGDB));
        for name in ["OpenFileGDB", "openfilegdb", "FileGDB", "gdb"] {
            assert_eq!(VectorDriver::from_name(name), Some(VectorDriver::FileGDB), "{name}");
        }
        let info = VectorDriver::FileGDB.info();
        assert_eq!((info.name, info.extensions, info.read, info.write), ("OpenFileGDB", &["gdb"][..], true, false));
        assert_eq!(crate::formats::canonical_driver_name("FileGDB"), "OPENFILEGDB");
    }

    #[cfg(not(feature = "gdal"))]
    #[test]
    fn writing_a_geodatabase_is_refused_without_the_gdal_backend() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.gdb");
        let e = crate::write_vector(&[Layer::new("a")], out.to_str().unwrap(), &Default::default()).unwrap_err();
        assert_eq!(e.to_string(), "the OpenFileGDB driver cannot write");
        assert!(!out.exists());
    }

    #[test]
    fn identification_and_info_recognize_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(&dir);
        let found = crate::inspection::identify(&path, false).unwrap().unwrap();
        assert_eq!(found.driver, "OpenFileGDB");
        assert_eq!(found.kinds, [Kind::Vector]);
        let crate::info::Info::Vector(info) = crate::info::info_path(&path, &Default::default()).unwrap() else { panic!("not vector") };
        assert_eq!(info.driver, "OpenFileGDB");
        assert_eq!(info.layers.len(), 6);
    }

    /// `.gdb` names without a geodatabase behind them are errors, and other directories are not
    /// taken for one.
    #[test]
    fn what_is_not_a_geodatabase_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.gdb");
        std::fs::create_dir(&empty).unwrap();
        let file = dir.path().join("file.gdb");
        std::fs::write(&file, b"not a directory").unwrap();
        for path in [&empty, &file] {
            let e = crate::open_vector(path.to_str().unwrap(), &Default::default()).unwrap_err().to_string();
            assert!(e.contains("is not a File Geodatabase"), "{e}");
            assert!(crate::inspection::identify(path.to_str().unwrap(), false).unwrap_or_default().is_none());
        }
        let missing = dir.path().join("missing.gdb");
        assert!(
            matches!(crate::open_vector(missing.to_str().unwrap(), &Default::default()), Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn bytes_are_refused_with_the_reason() {
        let e = super::super::read_bytes(b"x".to_vec(), "a.gdb", VectorDriver::FileGDB, &Default::default()).unwrap_err().to_string();
        assert!(e.contains("is a directory"), "{e}");
    }

    /// A table's layer writes out as other formats take it, null geometries and all.
    #[test]
    fn layers_convert_to_other_formats() {
        let dir = tempfile::tempdir().unwrap();
        let ds = opened(&sample(&dir), &Default::default());
        let out = dir.path().join("areas.geojson");
        crate::write_vector(std::slice::from_ref(layer(&ds, "Areas")), out.to_str().unwrap(), &Default::default()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let features = v["features"].as_array().unwrap();
        assert_eq!(features.len(), 3);
        assert_eq!(features[0]["properties"]["Label"], "first");
        assert_eq!(features[0]["geometry"]["type"], "MultiPolygon");
        assert!(features[1]["geometry"].is_null());
    }
}
