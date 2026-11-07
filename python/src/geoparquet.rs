//! `write_geoparquet_layers`: Arrow tables to one GeoParquet file, with the layout options of
//! `terrio::geoparquet`.

use crate::Detach;
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use terrio::GeoParquetWriteOptions;
use terrio::geoparquet::{ArrowLayer, MemoryLayer, split_by_column, write_arrow_layers_to_path};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(write_geoparquet_layers, m)?)?;
    Ok(())
}

/// The record batches of an object exporting Arrow data (or a GeoDataFrame, through `to_arrow()`).
fn arrow_of(obj: &Bound<'_, PyAny>) -> PyResult<(terrio::arrow::arrow_schema::SchemaRef, Vec<terrio::arrow::arrow_array::RecordBatch>)> {
    if let Some(x) = crate::arrow::read_capsules(obj)? {
        return Ok(x);
    }
    if obj.hasattr("to_arrow")?
        && let Some(x) = crate::arrow::read_capsules(&obj.call_method0("to_arrow")?)?
    {
        return Ok(x);
    }
    Err(invalid("expected an object with __arrow_c_stream__ or __arrow_c_array__ (a pyarrow Table, ...)"))
}

/// A `crs` argument as `geo` metadata: a PROJJSON dict, a PROJJSON or other CRS string, or `None`.
fn crs_value(py: Python<'_>, crs: Option<&Bound<'_, PyAny>>) -> PyResult<Option<serde_json::Value>> {
    let Some(c) = crs.filter(|c| !c.is_none()) else { return Ok(None) };
    Ok(Some(if let Ok(s) = c.extract::<String>() {
        match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v @ serde_json::Value::Object(_)) => v,
            _ => serde_json::Value::String(s),
        }
    } else {
        let text: String = py.import("json")?.call_method1("dumps", (c,))?.extract()?;
        serde_json::from_str(&text).map_err(|e| invalid(format!("crs: {e}")))?
    }))
}

/// Writes Arrow tables as one GeoParquet file.
///
/// The columns keep their Arrow types (float32, lists, structs, timestamps...); the geometry
/// column holds WKB (`binary`). Several layers share the file, told apart by a `LAYER_COLUMN`,
/// with the union of their fields as the schema.
///
/// Args:
///     layers: A pyarrow Table (or any object exporting Arrow data, or a GeoDataFrame) or a list
///         of `(name, table)` pairs. A single table holding the `LAYER_COLUMN` is split into
///         layers by its values, in first-appearance order.
///     path: The output file, replaced only once the new file is complete.
///     options: Layer creation options, as `terrio convert --lco` takes them for GeoParquet:
///         `COMPRESSION` (`SNAPPY` by default, `ZSTD`, `GZIP`, `BROTLI`, `LZ4_RAW`, `NONE`),
///         `COMPRESSION_LEVEL`, `ROW_GROUP_SIZE`, `GEOMETRY_NAME`, `WRITE_COVERING_BBOX`,
///         `COVERING_BBOX_NAME`, `LAYER_COLUMN`, `ROW_GROUP_PER_LAYER`, `SPATIAL_ORDER`
///         (`NONE` or `HILBERT`), `HILBERT_COLUMN`, `DICTIONARY` (`ALL`, `STRING` or `NONE`) and
///         `STATISTICS` (`PAGE` or `CHUNK`).
///     crs: The `crs` of the `geo` metadata: a PROJJSON dict or string, or another CRS string;
///         by default the geometry field's GeoArrow metadata, or the table's `geo` metadata.
///     identity: Columns identifying rows, which must not hold nulls: a list for every layer,
///         or a dict of lists by layer name. They are non-nullable when every layer has them so.
///
/// Returns:
///     dict: `rows`, `layers`, `row_groups` and `bbox` (or None).
#[pyfunction]
#[pyo3(signature = (layers, path, *, options=None, crs=None, identity=None))]
pub(crate) fn write_geoparquet_layers<'py>(
    py: Python<'py>,
    layers: &Bound<'py, PyAny>,
    path: std::path::PathBuf,
    options: Option<std::collections::BTreeMap<String, String>>,
    crs: Option<&Bound<'py, PyAny>>,
    identity: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyDict>> {
    let mut write = GeoParquetWriteOptions::default();
    for (k, v) in options.into_iter().flatten() {
        if !write.set(&k, &v).py()? {
            return Err(invalid(format!("unknown GeoParquet layer creation option '{k}'")));
        }
    }
    write.crs = crs_value(py, crs)?;

    let mut inputs: Vec<MemoryLayer> = Vec::new();
    if let Ok(list) = layers.cast::<PyList>() {
        for item in list.iter() {
            let pair = item.cast::<PyTuple>().map_err(|_| invalid("layers must be a table or a list of (name, table) pairs"))?;
            if pair.len() != 2 {
                return Err(invalid("layers must be a table or a list of (name, table) pairs"));
            }
            let name: Option<String> = pair.get_item(0)?.extract()?;
            let (schema, batches) = arrow_of(&pair.get_item(1)?)?;
            inputs.push(ArrowLayer::new(name, schema, batches.into_iter().map(Ok).collect()));
        }
    } else {
        let (schema, batches) = arrow_of(layers)?;
        match write.layer_column.as_deref().filter(|c| schema.index_of(c).is_ok()) {
            Some(column) => inputs = py.detach_result(|| split_by_column(&schema, &batches, column)).py()?,
            None => inputs.push(ArrowLayer::new(None, schema, batches.into_iter().map(Ok).collect())),
        }
    }
    if let Some(identity) = identity.filter(|i| !i.is_none()) {
        if let Ok(by_layer) = identity.cast::<PyDict>() {
            for (key, columns) in by_layer.iter() {
                let key: String = key.extract()?;
                let layer = inputs
                    .iter_mut()
                    .find(|l| l.name.as_deref() == Some(key.as_str()))
                    .ok_or_else(|| invalid(format!("identity names an unknown layer {key:?}")))?;
                layer.identity = columns.extract::<Vec<String>>()?;
            }
        } else {
            let columns: Vec<String> = identity.extract()?;
            for layer in &mut inputs {
                layer.identity = columns.clone();
            }
        }
    }

    let path = path.to_str().ok_or_else(|| invalid("the path is not valid Unicode"))?.to_string();
    let report = crate::with_warnings(py, move || write_arrow_layers_to_path(inputs, &path, &write))?;
    let out = PyDict::new(py);
    out.set_item("rows", report.rows)?;
    out.set_item("layers", report.layers)?;
    out.set_item("row_groups", report.row_groups)?;
    out.set_item("bbox", report.bbox.map(|b| b.to_vec()))?;
    Ok(out)
}
