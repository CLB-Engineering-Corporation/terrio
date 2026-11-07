//! GeoParquet files written from Arrow record batches, one or more layers per file, with the
//! layout options of [`GeoParquetWriteOptions`].
//!
//! [`write_arrow_layers`] writes Arrow layers (record batches with a WKB `Binary` geometry column)
//! into one GeoParquet 1.1 file. The columns keep their Arrow types (`Float32`, lists, structs,
//! timestamps...). [`crate::write_vector`] writes terrio [`Layer`](crate::Layer)s through it when a
//! GeoParquet layer creation option is given (several layers need a `LAYER_COLUMN`), with the
//! column types of the default GeoParquet writer.
//!
//! - **Layers.** Several inputs share one file, told apart by a `LAYER_COLUMN` (a non-null `Utf8`
//!   column holding each input's name, after the attributes). The schema is the union of the
//!   layers' fields in first-appearance order; a field missing from a layer is null there, and a
//!   `Null`-typed field takes the type another layer gives it (other type differences are refused).
//!   A field is non-nullable when every layer has it non-nullable; a layer's `identity` columns
//!   are checked for nulls and count as non-nullable. [`split_by_column`] turns one table with a
//!   layer column into layers.
//! - **Row groups** of at most `ROW_GROUP_SIZE` rows, ending at every layer boundary unless
//!   `ROW_GROUP_PER_LAYER=NO`.
//! - **Bounding-box covering** (GeoParquet 1.1, `WRITE_COVERING_BBOX`): a nullable struct of
//!   `Float64` `xmin`, `ymin`, `xmax`, `ymax`, null for null and empty geometries, named by
//!   `COVERING_BBOX_NAME`, with the `covering` metadata.
//! - **Hilbert order** within each layer (`SPATIAL_ORDER=HILBERT`; see [`hilbert_index`]), the key
//!   optionally stored in a `HILBERT_COLUMN`. A layer is buffered whole to be sorted.
//! - **Encoding**: the `COMPRESSION` codecs (Zstandard without the zstd C library unless the
//!   `libzstd` feature is on, see [`ParquetCompression::Zstd`]), `DICTIONARY` and `STATISTICS`.
//!
//! The CRS of the `geo` metadata is [`GeoParquetWriteOptions::crs`], else the geometry field's
//! GeoArrow extension metadata (`crs`), else the input schema's own `geo` metadata (as GeoPandas
//! exports it). With none, `crs` is `null` (unknown), never the specification's default of
//! OGC:CRS84.
//!
//! ```
//! use std::sync::Arc;
//! use terrio::arrow::arrow_array::{BinaryArray, Int64Array, RecordBatch};
//! use terrio::arrow::arrow_schema::{DataType, Field, Schema};
//! use terrio::geoparquet::{ArrowLayer, write_arrow_layers};
//! let point = |x: f64, y: f64| [&[1u8, 1, 0, 0, 0][..], &x.to_le_bytes(), &y.to_le_bytes()].concat();
//! let schema = Arc::new(Schema::new(vec![
//!     Field::new("id", DataType::Int64, false),
//!     Field::new("geometry", DataType::Binary, true),
//! ]));
//! let batch = RecordBatch::try_new(schema.clone(), vec![
//!     Arc::new(Int64Array::from(vec![1, 2])),
//!     Arc::new(BinaryArray::from_vec(vec![&point(0.0, 0.0), &point(5.0, 5.0)])),
//! ]).map_err(|e| terrio::Error::InvalidData(e.to_string()))?;
//! let mut options = terrio::GeoParquetWriteOptions::default();
//! for (k, v) in [("COMPRESSION", "ZSTD"), ("SPATIAL_ORDER", "HILBERT"), ("HILBERT_COLUMN", "hilbert_index")] {
//!     options.set(k, v)?;
//! }
//! options.crs = Some(serde_json::json!("EPSG:32633"));
//! let layers = vec![ArrowLayer::new(Some("points".into()), schema, vec![Ok(batch)])];
//! let mut out = Vec::new();
//! let report = write_arrow_layers(layers, &mut out, &options)?;
//! assert_eq!((report.rows, report.row_groups), (2, 1));
//! # Ok::<(), terrio::Error>(())
//! ```

pub use crate::drivers::vector::geoparquet_options::{
    ColumnStatistics, DictionaryColumns, GeoParquetWriteOptions, ParquetCompression, SpatialOrder,
};

use crate::{Error, Result};
use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, BinaryArray, Float64Array, RecordBatch, StringArray, StructArray, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef};
use parquet::arrow::ArrowSchemaConverter;
use parquet::arrow::arrow_writer::{ArrowLeafColumn, ArrowRowGroupWriterFactory, ArrowWriter, ArrowWriterOptions, compute_leaves};
use parquet::basic::{BrotliLevel, Compression, GzipLevel, LogicalType};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::ColumnPath;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// The four bounding-box members, in GeoParquet covering order.
pub const BBOX_FIELDS: [&str; 4] = ["xmin", "ymin", "xmax", "ymax"];
/// The extension name of WKB geometry columns.
const WKB_EXTENSION: &str = "geoarrow.wkb";

fn err(e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("GeoParquet: {e}"))
}

fn arg(e: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("GeoParquet: {e}"))
}

/// One input layer: a name (for the layer column), its schema, its record batches and the columns
/// identifying its rows.
pub struct ArrowLayer<I> {
    /// The layer's name, written in the layer column.
    pub name: Option<String>,
    /// The schema of the batches.
    pub schema: SchemaRef,
    /// The record batches.
    pub batches: I,
    /// Columns that identify the rows (for example a mesh name and a cell id): they must not hold
    /// nulls, and are non-nullable in the output when every layer has them so.
    pub identity: Vec<String>,
}

impl<I> ArrowLayer<I> {
    /// A layer without identity columns.
    pub fn new(name: Option<String>, schema: SchemaRef, batches: I) -> Self {
        Self { name, schema, batches, identity: Vec::new() }
    }

    /// The layer with identity columns.
    pub fn with_identity(mut self, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.identity = columns.into_iter().map(Into::into).collect();
        self
    }
}

/// An in-memory layer.
pub type MemoryLayer = ArrowLayer<Vec<Result<RecordBatch>>>;

/// Splits a table into layers by the values of its `Utf8` (or `LargeUtf8`) `column`, in
/// first-appearance order, keeping the input order within each layer and leaving the column out.
/// The batches of a layer are zero-copy slices of the input's.
pub fn split_by_column(schema: &SchemaRef, batches: &[RecordBatch], column: &str) -> Result<Vec<MemoryLayer>> {
    let index = schema.index_of(column).map_err(|_| arg(format!("layer column {column:?} is not in the table")))?;
    let keep: Vec<usize> = (0..schema.fields().len()).filter(|&i| i != index).collect();
    let projected = Arc::new(schema.project(&keep).map_err(err)?);
    let mut names: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut runs: Vec<Vec<RecordBatch>> = Vec::new();
    for batch in batches {
        let values = batch.column(index);
        if values.null_count() > 0 {
            return Err(arg(format!("layer column {column:?} holds nulls")));
        }
        let value = |i: usize| -> Result<&str> {
            match values.data_type() {
                DataType::Utf8 => Ok(values.as_string::<i32>().value(i)),
                DataType::LargeUtf8 => Ok(values.as_string::<i64>().value(i)),
                other => Err(arg(format!("layer column {column:?} is {other}, not Utf8"))),
            }
        };
        let body = batch.project(&keep).map_err(err)?;
        let mut start = 0;
        while start < batch.num_rows() {
            let name = value(start)?;
            let mut end = start + 1;
            while end < batch.num_rows() && value(end)? == name {
                end += 1;
            }
            let layer = match by_name.get(name) {
                Some(&l) => l,
                None => {
                    by_name.insert(name.to_string(), names.len());
                    names.push(name.to_string());
                    runs.push(Vec::new());
                    names.len() - 1
                }
            };
            runs[layer].push(body.slice(start, end - start));
            start = end;
        }
    }
    Ok(names
        .into_iter()
        .zip(runs)
        .map(|(name, batches)| ArrowLayer::new(Some(name), projected.clone(), batches.into_iter().map(Ok).collect()))
        .collect())
}

/// What [`write_arrow_layers`] wrote.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct GeoParquetWriteReport {
    /// The number of rows.
    pub rows: usize,
    /// The number of layers.
    pub layers: usize,
    /// The number of row groups.
    pub row_groups: usize,
    /// The extent of the geometries, `[xmin, ymin, xmax, ymax]`.
    pub bbox: Option<[f64; 4]>,
}

/// The output schema and, per layer, where its columns come from.
struct Plan {
    schema: SchemaRef,
    /// Per layer, per output field: the input column index.
    sources: Vec<Vec<Option<usize>>>,
    /// Per layer: the indexes of its identity columns.
    identities: Vec<Vec<usize>>,
    layer_column: Option<usize>,
    bbox: Option<usize>,
    hilbert: Option<usize>,
    /// The `crs` of the `geo` metadata; `None` leaves it out (OGC:CRS84).
    crs: Option<Value>,
}

/// The `crs` of a geometry field's GeoArrow metadata, or of a schema's `geo` metadata.
fn input_crs(schema: &Schema, geometry: &str) -> Result<Option<Value>> {
    let field = schema.field_with_name(geometry).map_err(err)?;
    if let Some(meta) = field.metadata().get(crate::arrow::EXTENSION_METADATA) {
        let meta: Value = serde_json::from_str(meta).map_err(|e| err(format!("geometry extension metadata: {e}")))?;
        if let Some(crs) = meta.get("crs").filter(|c| !c.is_null()) {
            // PROJJSON as an object, or (`crs_type: "projjson"`) as a JSON string.
            return Ok(Some(match (crs, meta.get("crs_type").and_then(Value::as_str)) {
                (Value::String(s), Some("projjson")) => serde_json::from_str(s).map_err(|e| err(format!("PROJJSON: {e}")))?,
                (c, _) => c.clone(),
            }));
        }
    }
    if let Some(geo) = schema.metadata().get("geo") {
        let geo: Value = serde_json::from_str(geo).map_err(|e| err(format!("geo metadata: {e}")))?;
        if let Some(crs) = geo.get("columns").and_then(|c| c.get(geometry)).and_then(|c| c.get("crs")) {
            return Ok(Some(crs.clone()));
        }
    }
    Ok(None)
}

fn bbox_struct_fields() -> Fields {
    BBOX_FIELDS.iter().map(|n| Field::new(*n, DataType::Float64, true)).collect()
}

/// Where the `crs` of the `geo` metadata comes from.
enum CrsSource {
    /// The options, else the inputs (`null` when none has one).
    Inputs,
    /// Given by the caller: `None` leaves `crs` out.
    Given(Option<Value>),
}

fn plan<I>(layers: &[ArrowLayer<I>], options: &GeoParquetWriteOptions, crs_source: CrsSource) -> Result<Plan> {
    if layers.is_empty() {
        return Err(arg("at least one layer is required"));
    }
    if options.row_group_size == 0 {
        return Err(arg("ROW_GROUP_SIZE must be positive"));
    }
    options.compression.check_level(options.compression_level)?;
    match &options.layer_column {
        None if layers.len() > 1 => return Err(arg("a file holds one layer unless a LAYER_COLUMN tells the layers apart")),
        Some(column) => {
            if column.is_empty() {
                return Err(arg("LAYER_COLUMN needs a name"));
            }
            let mut seen = std::collections::HashSet::new();
            for layer in layers {
                let name = layer.name.as_deref().ok_or_else(|| arg("a LAYER_COLUMN needs a name for every layer"))?;
                if name.is_empty() || !seen.insert(name) {
                    return Err(arg("layer names must be nonempty and unique"));
                }
            }
        }
        None => {}
    }
    if options.hilbert_column.is_some() && options.spatial_order != SpatialOrder::Hilbert {
        return Err(arg("HILBERT_COLUMN needs SPATIAL_ORDER=HILBERT"));
    }
    let geometry = options.geometry_column.as_str();
    let mut crs: Option<Value> = options.crs.clone();
    let mut fields: Vec<Field> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    // Per field: whether every layer has it, non-nullable.
    let mut required: Vec<bool> = Vec::new();
    for (l, layer) in layers.iter().enumerate() {
        let schema = &layer.schema;
        let g = schema.field_with_name(geometry).map_err(|_| arg(format!("layer {l} has no {geometry:?} column")))?;
        if g.data_type() != &DataType::Binary {
            return Err(arg(format!("the geometry column must be Binary (WKB), not {}", g.data_type())));
        }
        if matches!(crs_source, CrsSource::Inputs) && options.crs.is_none() {
            let found = input_crs(schema, geometry)?;
            match (&crs, found) {
                (_, None) => {}
                (None, Some(c)) if l == 0 => crs = Some(c),
                (Some(a), Some(b)) if *a == b => {}
                _ => return Err(arg("the layers have different CRSs")),
            }
        }
        for name in &layer.identity {
            if name == geometry || schema.field_with_name(name).is_err() {
                return Err(arg(format!("identity column {name:?} is not an attribute of layer {l}")));
            }
        }
        for field in schema.fields() {
            let non_null = !field.is_nullable() || layer.identity.iter().any(|n| n == field.name());
            match index.get(field.name()) {
                Some(&i) => {
                    let (a, b) = (fields[i].data_type(), field.data_type());
                    if a != b {
                        if a == &DataType::Null {
                            fields[i] = fields[i].clone().with_data_type(b.clone());
                        } else if b != &DataType::Null {
                            return Err(arg(format!("field {} is {a} in one layer and {b} in another", field.name())));
                        }
                    }
                    required[i] &= non_null;
                }
                None => {
                    index.insert(field.name().clone(), fields.len());
                    fields.push(field.as_ref().clone());
                    // A field first seen after the first layer is missing (null) in an earlier one.
                    required.push(non_null && l == 0);
                }
            }
        }
        for (i, f) in fields.iter().enumerate() {
            if schema.field_with_name(f.name()).is_err() {
                required[i] = false;
            }
        }
    }
    for (f, r) in fields.iter_mut().zip(&required) {
        *f = f.clone().with_nullable(!r);
    }
    let geometry_index = *index.get(geometry).ok_or_else(|| arg(format!("no {geometry:?} column")))?;
    let nullable = fields[geometry_index].is_nullable();
    fields[geometry_index] = Field::new(geometry, DataType::Binary, nullable)
        .with_metadata(HashMap::from([(crate::arrow::EXTENSION_NAME.to_string(), WKB_EXTENSION.to_string())]));
    let mut added: Vec<String> = Vec::new();
    let layer_column = options.layer_column.as_ref().map(|name| {
        added.push(name.clone());
        fields.push(Field::new(name, DataType::Utf8, false));
        fields.len() - 1
    });
    let bbox = options.write_covering_bbox.then(|| {
        let name = options.covering_name();
        added.push(name.clone());
        fields.push(Field::new(name, DataType::Struct(bbox_struct_fields()), true));
        fields.len() - 1
    });
    let hilbert = options.hilbert_column.as_ref().map(|name| {
        added.push(name.clone());
        fields.push(Field::new(name, DataType::UInt64, true));
        fields.len() - 1
    });
    for name in &added {
        if name.is_empty() {
            return Err(arg("generated columns need names"));
        }
        if fields.iter().filter(|f| f.name().eq_ignore_ascii_case(name)).count() > 1 {
            return Err(arg(format!("input field {name} collides with a generated column")));
        }
    }
    let mut metadata = layers[0].schema.metadata().clone();
    metadata.remove("geo");
    let schema = Arc::new(Schema::new_with_metadata(fields, metadata));
    let sources = layers.iter().map(|l| schema.fields().iter().map(|f| l.schema.index_of(f.name()).ok()).collect()).collect();
    let identities = layers
        .iter()
        .map(|l| l.identity.iter().map(|n| l.schema.index_of(n).map_err(err)).collect::<Result<Vec<_>>>())
        .collect::<Result<Vec<_>>>()?;
    let crs = match crs_source {
        CrsSource::Inputs => Some(crs.unwrap_or(Value::Null)),
        CrsSource::Given(c) => options.crs.clone().or(c),
    };
    Ok(Plan { schema, sources, identities, layer_column, bbox, hilbert, crs })
}

// ---------------------------------------------------------------------------------------------
// WKB bounds and types
// ---------------------------------------------------------------------------------------------

/// The ISO WKB geometry type names, by base type code.
const TYPE_NAMES: [&str; 8] = ["", "Point", "LineString", "Polygon", "MultiPoint", "MultiLineString", "MultiPolygon", "GeometryCollection"];

struct WkbReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl WkbReader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.at.checked_add(N).filter(|&e| e <= self.bytes.len()).ok_or_else(|| err("truncated WKB"))?;
        let out = self.bytes[self.at..end].try_into().map_err(err)?;
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self, le: bool) -> Result<u32> {
        let b = self.take::<4>()?;
        Ok(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn f64(&mut self, le: bool) -> Result<f64> {
        let b = self.take::<8>()?;
        Ok(if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }

    /// A geometry header: byte order, base type, coordinate dimension and whether it has Z.
    fn header(&mut self) -> Result<(bool, u32, usize, bool)> {
        let le = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            b => return Err(err(format!("WKB byte order {b}"))),
        };
        let code = self.u32(le)?;
        // EWKB flags (Z, M, SRID) or ISO thousands.
        let (mut z, mut m) = (code & 0x8000_0000 != 0, code & 0x4000_0000 != 0);
        if code & 0x2000_0000 != 0 {
            self.u32(le)?;
        }
        let iso = code & 0x0FFF_FFFF;
        let base = iso % 1000;
        match iso / 1000 {
            0 => {}
            1 => z = true,
            2 => m = true,
            3 => (z, m) = (true, true),
            _ => return Err(err(format!("WKB geometry type {code}"))),
        }
        if !(1..=7).contains(&base) {
            return Err(err(format!("WKB geometry type {code}")));
        }
        Ok((le, base, 2 + usize::from(z) + usize::from(m), z))
    }

    fn points(&mut self, le: bool, dims: usize, n: usize, b: &mut Option<[f64; 4]>) -> Result<()> {
        for _ in 0..n {
            let (x, y) = (self.f64(le)?, self.f64(le)?);
            for _ in 2..dims {
                self.f64(le)?;
            }
            // NaN coordinates (the empty point) are no coordinates.
            if x.is_nan() || y.is_nan() {
                continue;
            }
            let e = b.get_or_insert([x, y, x, y]);
            e[0] = e[0].min(x);
            e[1] = e[1].min(y);
            e[2] = e[2].max(x);
            e[3] = e[3].max(y);
        }
        Ok(())
    }

    fn count(&mut self, le: bool) -> Result<usize> {
        let n = self.u32(le)? as usize;
        // Each element takes at least one byte: refuse counts the data cannot hold.
        if n > self.bytes.len() - self.at {
            return Err(err("truncated WKB"));
        }
        Ok(n)
    }

    fn geometry(&mut self, b: &mut Option<[f64; 4]>, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(err("WKB nested too deeply"));
        }
        let (le, base, dims, _) = self.header()?;
        match base {
            1 => self.points(le, dims, 1, b),
            2 => {
                let n = self.count(le)?;
                self.points(le, dims, n, b)
            }
            3 => {
                for _ in 0..self.count(le)? {
                    let n = self.count(le)?;
                    self.points(le, dims, n, b)?;
                }
                Ok(())
            }
            _ => {
                for _ in 0..self.count(le)? {
                    self.geometry(b, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}

/// The bounds `[xmin, ymin, xmax, ymax]` of a WKB (ISO or EWKB, either byte order) geometry over
/// all its coordinates (holes and parts included; Z and M ignored), or `None` for an empty one.
/// Truncated or unknown WKB is an error.
pub fn wkb_bounds(wkb: &[u8]) -> Result<Option<[f64; 4]>> {
    let mut b = None;
    let mut r = WkbReader { bytes: wkb, at: 0 };
    r.geometry(&mut b, 0)?;
    Ok(b)
}

/// The GeoParquet `geometry_types` name of a WKB geometry (`Polygon`, `Point Z`...).
fn wkb_type_name(wkb: &[u8]) -> Result<String> {
    let (_, base, _, z) = WkbReader { bytes: wkb, at: 0 }.header()?;
    let name = TYPE_NAMES[base as usize];
    Ok(if z { format!("{name} Z") } else { name.to_string() })
}

// ---------------------------------------------------------------------------------------------
// Hilbert order
// ---------------------------------------------------------------------------------------------

/// The Hilbert index of `(x, y)` on a 65,536 x 65,536 grid over `extent = [xmin, ymin, xmax,
/// ymax]`, the key of DuckDB spatial's `ST_Hilbert(x, y, box)`: each coordinate is scaled to
/// `(v - min) * (65535 / (max - min))` and truncated to an integer (0 on an axis of no width), and
/// the cell's distance along the curve is returned. Coordinates outside the extent are clamped to
/// its edge cells; DuckDB's keys differ there, but the writer never computes them, since every
/// bounding-box centre is inside the layer's extent.
///
/// The curve is the one `terrio sort` and GDAL 3.13's `gdal vector sort --method hilbert` use
/// (GDAL's `GDALHilbertCode`, see [`crate::ops::vector_geom`]); DuckDB spatial uses the same curve.
/// Only the grid differs from `terrio sort`, which scales by 65534 and rounds. The keys equal
/// DuckDB spatial's (DuckDB 1.4.4 and 2.0.0) on the points this module's tests check, which DuckDB
/// computed.
pub fn hilbert_index(x: f64, y: f64, extent: [f64; 4]) -> u32 {
    fn cell(v: f64, min: f64, max: f64) -> u32 {
        let width = max - min;
        // `as` truncates (NaN to 0).
        if width > 0.0 { ((v - min) * (65535.0 / width)).clamp(0.0, 65535.0) as u32 } else { 0 }
    }
    crate::ops::vector_geom::hilbert(cell(x, extent[0], extent[2]), cell(y, extent[1], extent[3]))
}

/// The Hilbert index of each bounding box's centre over the union of the boxes; `None` for a
/// missing box (null or empty geometry).
pub fn hilbert_keys(bounds: &[Option<[f64; 4]>]) -> Vec<Option<u32>> {
    let mut extent: Option<[f64; 4]> = None;
    for b in bounds.iter().flatten() {
        let e = extent.get_or_insert(*b);
        e[0] = e[0].min(b[0]);
        e[1] = e[1].min(b[1]);
        e[2] = e[2].max(b[2]);
        e[3] = e[3].max(b[3]);
    }
    bounds
        .iter()
        .map(|b| match (b, extent) {
            (Some(b), Some(e)) => Some(hilbert_index((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, e)),
            _ => None,
        })
        .collect()
}

/// The stable order of the rows by key, rows without a key last.
pub fn hilbert_permutation(keys: &[Option<u32>]) -> Vec<usize> {
    let mut keyed: Vec<(u64, usize)> = keys.iter().enumerate().map(|(i, k)| (k.map_or(u64::MAX, u64::from), i)).collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, i)| i).collect()
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Per-row bounds and the type names seen in a chunk of rows.
type ScannedChunk = (Vec<Option<[f64; 4]>>, Vec<String>);

/// The geometries of a batch: per-row bounds and the type names seen.
fn scan_geometry(array: &BinaryArray, types: &mut Vec<String>) -> Result<Vec<Option<[f64; 4]>>> {
    const CHUNK: usize = 4096;
    let parts = crate::par::map_range(array.len().div_ceil(CHUNK), |c| -> Result<ScannedChunk> {
        let (start, end) = (c * CHUNK, ((c + 1) * CHUNK).min(array.len()));
        let mut bounds = Vec::with_capacity(end - start);
        let mut names: Vec<String> = Vec::new();
        let mut last: Option<[u8; 5]> = None;
        for i in start..end {
            if array.is_null(i) {
                bounds.push(None);
                continue;
            }
            let wkb = array.value(i);
            bounds.push(wkb_bounds(wkb)?);
            // The type name, unless the header is the previous row's.
            let head: Option<[u8; 5]> = wkb.get(..5).and_then(|h| h.try_into().ok());
            if head.is_none() || head != last {
                let name = wkb_type_name(wkb)?;
                if !names.contains(&name) {
                    names.push(name);
                }
                last = head;
            }
        }
        Ok((bounds, names))
    });
    let mut out = Vec::with_capacity(array.len());
    for part in parts {
        let (b, names) = part?;
        out.extend(b);
        for n in names {
            if !types.contains(&n) {
                types.push(n);
            }
        }
    }
    Ok(out)
}

fn bbox_member(bounds: &[Option<[f64; 4]>], k: usize) -> ArrayRef {
    Arc::new(Float64Array::from_iter(bounds.iter().map(|b| b.map(|b| b[k]))))
}

/// The output batch of a layer's input batch.
fn conform(plan: &Plan, l: usize, name: Option<&str>, batch: &RecordBatch, bounds: &[Option<[f64; 4]>]) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let mut out: Vec<ArrayRef> = Vec::with_capacity(plan.schema.fields().len());
    for (i, field) in plan.schema.fields().iter().enumerate() {
        let array: ArrayRef = if Some(i) == plan.layer_column {
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n(name.unwrap_or_default(), rows)))
        } else if Some(i) == plan.hilbert {
            // Filled once the layer is ordered.
            arrow_array::new_null_array(&DataType::UInt64, rows)
        } else if Some(i) == plan.bbox {
            let validity = arrow_buffer::NullBuffer::from_iter(bounds.iter().map(Option::is_some));
            Arc::new(
                StructArray::try_new(bbox_struct_fields(), (0..4).map(|k| bbox_member(bounds, k)).collect(), Some(validity))
                    .map_err(err)?,
            )
        } else {
            match plan.sources[l][i] {
                Some(s) if batch.column(s).data_type() == field.data_type() => batch.column(s).clone(),
                // A Null-typed column of this layer, typed in another, or a missing one.
                _ => arrow_array::new_null_array(field.data_type(), rows),
            }
        };
        out.push(array);
    }
    RecordBatch::try_new(plan.schema.clone(), out).map_err(err)
}

/// Row groups written as `ArrowWriter` writes them, their columns encoded in parallel.
struct RowGroups<W: std::io::Write + Send> {
    writer: SerializedFileWriter<W>,
    factory: ArrowRowGroupWriterFactory,
    schema: SchemaRef,
    limit: usize,
    /// The Zstandard level, when the pages are compressed here (see `geoparquet_zstd`).
    zstd: Option<i32>,
    pending: Vec<RecordBatch>,
    rows: usize,
    groups: usize,
}

impl<W: std::io::Write + Send> RowGroups<W> {
    fn push(&mut self, mut batch: RecordBatch) -> Result<()> {
        while batch.num_rows() > 0 {
            let take = batch.num_rows().min(self.limit - self.rows);
            self.pending.push(batch.slice(0, take));
            self.rows += take;
            batch = batch.slice(take, batch.num_rows() - take);
            if self.rows == self.limit {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut by_column: Vec<Vec<ArrowLeafColumn>> = Vec::new();
        for batch in std::mem::take(&mut self.pending) {
            let mut k = 0;
            for (array, field) in batch.columns().iter().zip(self.schema.fields()) {
                for leaf in compute_leaves(field, array).map_err(err)? {
                    if by_column.len() == k {
                        by_column.push(Vec::new());
                    }
                    by_column[k].push(leaf);
                    k += 1;
                }
            }
        }
        let writers = self.factory.create_column_writers(self.groups).map_err(err)?;
        let work: Vec<std::sync::Mutex<Option<_>>> = writers.into_iter().zip(by_column).map(|w| std::sync::Mutex::new(Some(w))).collect();
        let chunks = crate::par::map_range(work.len(), |i| {
            let Some((mut writer, leaves)) = work[i].lock().ok().and_then(|mut w| w.take()) else {
                return Err(err("column writer"));
            };
            for leaf in &leaves {
                writer.write(leaf).map_err(err)?;
            }
            writer.close().map_err(err)
        });
        let chunks = chunks.into_iter().collect::<Result<Vec<_>>>()?;
        let (root, props) = (self.writer.schema_descr().root_schema_ptr(), self.writer.properties().clone());
        let mut group = self.writer.next_row_group().map_err(err)?;
        match self.zstd {
            Some(level) => crate::drivers::vector::geoparquet_zstd::append_zstd(chunks, &mut group, root, props, level).map_err(err)?,
            None => {
                for chunk in chunks {
                    chunk.append_to_row_group(&mut group).map_err(err)?;
                }
            }
        }
        group.close().map_err(err)?;
        self.groups += 1;
        self.rows = 0;
        Ok(())
    }
}

/// The codec the `parquet` crate writes with, and the Zstandard level when the pages are
/// compressed by `geoparquet_zstd` instead.
fn codec(options: &GeoParquetWriteOptions) -> Result<(Compression, Option<i32>)> {
    let level = options.compression_level;
    Ok(match options.compression {
        ParquetCompression::Uncompressed => (Compression::UNCOMPRESSED, None),
        ParquetCompression::Snappy => (Compression::SNAPPY, None),
        ParquetCompression::Gzip => (Compression::GZIP(GzipLevel::try_new(level.map_or(6, |l| l as u32)).map_err(arg)?), None),
        ParquetCompression::Brotli => (Compression::BROTLI(BrotliLevel::try_new(level.map_or(1, |l| l as u32)).map_err(arg)?), None),
        ParquetCompression::Lz4Raw => (Compression::LZ4_RAW, None),
        ParquetCompression::Zstd => (Compression::UNCOMPRESSED, Some(level.unwrap_or(1))),
    })
}

fn writer_properties(plan: &Plan, options: &GeoParquetWriteOptions, compression: Compression) -> Result<WriterProperties> {
    let mut props = WriterProperties::builder()
        .set_compression(compression)
        .set_max_row_group_row_count(Some(options.row_group_size))
        // The specification's (and pyarrow's) list layout: three-level LIST, items `element`.
        .set_coerce_types(true);
    match options.dictionary {
        DictionaryColumns::All => {}
        DictionaryColumns::None => props = props.set_dictionary_enabled(false),
        DictionaryColumns::String => {
            props = props.set_dictionary_enabled(false);
            for f in plan.schema.fields().iter().filter(|f| matches!(f.data_type(), DataType::Utf8 | DataType::LargeUtf8)) {
                props = props.set_column_dictionary_enabled(ColumnPath::from(f.name().as_str()), true);
            }
        }
    }
    if options.statistics == ColumnStatistics::Chunk {
        props = props.set_statistics_enabled(EnabledStatistics::Chunk).set_statistics_truncate_length(None).set_offset_index_disabled(true);
        // No statistics for all-null (`Null`, Parquet UNKNOWN) columns, nested ones included.
        let leaves = ArrowSchemaConverter::new().with_coerce_types(true).convert(&plan.schema).map_err(err)?;
        for leaf in leaves.columns() {
            if leaf.logical_type_ref() == Some(&LogicalType::Unknown) {
                props = props.set_column_statistics_enabled(leaf.path().clone(), EnabledStatistics::None);
            }
        }
    }
    Ok(props.build())
}

/// The `geo` metadata.
fn geo_metadata(plan: &Plan, options: &GeoParquetWriteOptions, mut types: Vec<String>, bbox: Option<[f64; 4]>) -> Value {
    types.sort();
    let mut column = Map::new();
    column.insert("encoding".into(), json!("WKB"));
    column.insert("geometry_types".into(), json!(types));
    if let Some(crs) = &plan.crs {
        column.insert("crs".into(), crs.clone());
    }
    if let Some(b) = bbox {
        column.insert("bbox".into(), json!(b));
    }
    if options.write_covering_bbox {
        let name = options.covering_name();
        let path = |k: usize| json!([name, BBOX_FIELDS[k]]);
        column.insert("covering".into(), json!({"bbox": {"xmin": path(0), "ymin": path(1), "xmax": path(2), "ymax": path(3)}}));
    }
    json!({
        "version": "1.1.0",
        "primary_column": options.geometry_column,
        "columns": {options.geometry_column.clone(): Value::Object(column)},
    })
}

/// Writes Arrow layers as one GeoParquet file into `out` (see the [module](self) documentation).
/// Layers are written in order; with Hilbert order one layer is buffered at a time, otherwise one
/// batch.
pub fn write_arrow_layers<I, W>(layers: Vec<ArrowLayer<I>>, out: W, options: &GeoParquetWriteOptions) -> Result<GeoParquetWriteReport>
where
    I: IntoIterator<Item = Result<RecordBatch>>,
    W: std::io::Write + Send,
{
    write_layers(layers, out, options, CrsSource::Inputs).map(|(report, _)| report)
}

/// [`write_arrow_layers`] into a local file, which is replaced only once the new file is complete.
pub fn write_arrow_layers_to_path<I>(
    layers: Vec<ArrowLayer<I>>,
    path: &str,
    options: &GeoParquetWriteOptions,
) -> Result<GeoParquetWriteReport>
where
    I: IntoIterator<Item = Result<RecordBatch>>,
{
    to_path(path, |out| write_layers(layers, out, options, CrsSource::Inputs).map(|(report, _)| report))
}

/// Writes a file with `write`, replacing `path` atomically.
fn to_path(
    path: &str,
    write: impl FnOnce(&mut std::io::BufWriter<&mut std::fs::File>) -> Result<GeoParquetWriteReport>,
) -> Result<GeoParquetWriteReport> {
    crate::dataset::ensure_writable_location(path)?;
    let mut report = None;
    crate::vsi::write_local_atomic(std::path::Path::new(path), |file| {
        let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
        report = Some(write(&mut out)?);
        std::io::Write::flush(&mut out)?;
        Ok(())
    })?;
    report.ok_or_else(|| err("no report"))
}

/// Writes terrio layers as one GeoParquet file into `out`: each layer converted to Arrow with the
/// column types of the default GeoParquet writer, and the `crs` it writes.
pub(crate) fn write_vector_layers<W: std::io::Write + Send>(
    layers: &[crate::Layer],
    out: W,
    options: &GeoParquetWriteOptions,
) -> Result<GeoParquetWriteReport> {
    use crate::drivers::vector::geoparquet::{ParquetEncoder, geo_crs};
    use crate::drivers::vector::profile::LayerProfile;
    let mut crs: Option<Option<Value>> = None;
    let mut inputs: Vec<MemoryLayer> = Vec::with_capacity(layers.len());
    for layer in layers {
        let profile = if ParquetEncoder::needs_profile(layer) { LayerProfile::of(layer) } else { LayerProfile::new(layer) };
        let enc = ParquetEncoder::new(layer, &profile);
        let layer_crs = geo_crs(layer);
        match &crs {
            None => crs = Some(layer_crs),
            Some(c) if *c == layer_crs => {}
            Some(_) if options.crs.is_some() => {}
            Some(_) => return Err(arg("the layers have different CRSs")),
        }
        // The default writer's columns without its covering, which is written here.
        let (schema, batches) = enc.attribute_batches(&layer.features, &options.geometry_column)?;
        inputs.push(ArrowLayer::new(Some(layer.name.clone()), schema, batches.into_iter().map(Ok).collect()));
    }
    write_layers(inputs, out, options, CrsSource::Given(crs.flatten())).map(|(report, _)| report)
}

/// [`write_vector_layers`] into a local file, replaced atomically.
pub(crate) fn write_vector_layers_to_path(
    layers: &[crate::Layer],
    path: &str,
    options: &GeoParquetWriteOptions,
) -> Result<GeoParquetWriteReport> {
    to_path(path, |out| write_vector_layers(layers, out, options))
}

fn write_layers<I, W>(
    layers: Vec<ArrowLayer<I>>,
    out: W,
    options: &GeoParquetWriteOptions,
    crs_source: CrsSource,
) -> Result<(GeoParquetWriteReport, W)>
where
    I: IntoIterator<Item = Result<RecordBatch>>,
    W: std::io::Write + Send,
{
    let plan = plan(&layers, options, crs_source)?;
    let (compression, zstd) = codec(options)?;
    let props = writer_properties(&plan, options, compression)?;
    // The Arrow schema is written at the end, so that it carries the `geo` metadata as pyarrow
    // and GDAL write it.
    let arrow = ArrowWriter::try_new_with_options(
        out,
        plan.schema.clone(),
        ArrowWriterOptions::new().with_properties(props).with_skip_arrow_metadata(true),
    )
    .map_err(err)?;
    let (writer, factory) = arrow.into_serialized_writer().map_err(err)?;
    let mut groups = RowGroups {
        writer,
        factory,
        schema: plan.schema.clone(),
        limit: options.row_group_size,
        zstd,
        pending: Vec::new(),
        rows: 0,
        groups: 0,
    };
    let mut report = GeoParquetWriteReport::default();
    let mut types: Vec<String> = Vec::new();
    let mut extent: Option<[f64; 4]> = None;
    let geometry_column = options.geometry_column.as_str();
    for (l, layer) in layers.into_iter().enumerate() {
        let name = layer.name.as_deref();
        let input_geometry = layer.schema.index_of(geometry_column).map_err(err)?;
        let mut buffered: Vec<RecordBatch> = Vec::new();
        let mut layer_bounds: Vec<Option<[f64; 4]>> = Vec::new();
        for batch in layer.batches {
            let batch = batch?;
            if batch.schema().fields() != layer.schema.fields() {
                return Err(arg(format!("a batch of layer {l} does not match its schema")));
            }
            for &c in &plan.identities[l] {
                if batch.column(c).null_count() > 0 {
                    return Err(arg(format!("identity column {:?} of layer {l} holds nulls", layer.schema.field(c).name())));
                }
            }
            let wkb = batch.column(input_geometry).as_binary_opt::<i32>().ok_or_else(|| arg("the geometry column is not Binary"))?;
            let bounds = scan_geometry(wkb, &mut types)?;
            for b in bounds.iter().flatten() {
                let e = extent.get_or_insert(*b);
                e[0] = e[0].min(b[0]);
                e[1] = e[1].min(b[1]);
                e[2] = e[2].max(b[2]);
                e[3] = e[3].max(b[3]);
            }
            let conformed = conform(&plan, l, name, &batch, &bounds)?;
            report.rows += batch.num_rows();
            if options.spatial_order == SpatialOrder::Hilbert {
                layer_bounds.extend(bounds);
                if conformed.num_rows() > 0 {
                    buffered.push(conformed);
                }
            } else {
                groups.push(conformed)?;
            }
        }
        if !buffered.is_empty() {
            let keys = hilbert_keys(&layer_bounds);
            let order = hilbert_permutation(&keys);
            let mut locations = Vec::with_capacity(order.len());
            for (b, batch) in buffered.iter().enumerate() {
                locations.extend((0..batch.num_rows()).map(|r| (b, r)));
            }
            let refs: Vec<&RecordBatch> = buffered.iter().collect();
            for chunk in order.chunks(options.row_group_size) {
                // One batch: `take`, much faster than `interleave` for lists and structs.
                let mut sorted = if let [only] = refs.as_slice() {
                    let indices = UInt32Array::from(chunk.iter().map(|&i| u32::try_from(i).map_err(err)).collect::<Result<Vec<_>>>()?);
                    arrow_select::take::take_record_batch(only, &indices).map_err(err)?
                } else {
                    let indices: Vec<(usize, usize)> = chunk.iter().map(|&i| locations[i]).collect();
                    arrow_select::interleave::interleave_record_batch(&refs, &indices).map_err(err)?
                };
                if let Some(h) = plan.hilbert {
                    let mut columns = sorted.columns().to_vec();
                    columns[h] = Arc::new(UInt64Array::from_iter(chunk.iter().map(|&i| keys[i].map(u64::from))));
                    sorted = RecordBatch::try_new(plan.schema.clone(), columns).map_err(err)?;
                }
                groups.push(sorted)?;
            }
        }
        if options.row_group_per_layer {
            groups.flush()?;
        }
        report.layers += 1;
    }
    groups.flush()?;
    let geo = geo_metadata(&plan, options, types, extent).to_string();
    let mut schema_metadata = plan.schema.metadata().clone();
    schema_metadata.insert("geo".into(), geo.clone());
    let mut writer = groups.writer;
    writer.append_key_value_metadata(KeyValue::new(
        parquet::arrow::ARROW_SCHEMA_META_KEY.into(),
        parquet::arrow::encode_arrow_schema(&plan.schema.as_ref().clone().with_metadata(schema_metadata)),
    ));
    writer.append_key_value_metadata(KeyValue::new("geo".into(), geo));
    let out = writer.into_inner().map_err(err)?;
    report.row_groups = groups.groups;
    report.bbox = extent;
    Ok((report, out))
}

#[cfg(test)]
#[path = "geoparquet_tests.rs"]
mod tests;
