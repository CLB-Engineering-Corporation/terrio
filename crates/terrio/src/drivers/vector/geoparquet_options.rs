// Written to match the behavior of (option names, values and defaults):
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/parquet/ogrparquetdriver.cpp
//     ogr/ogrsf_frmts/parquet/ogrparquetwriterlayer.cpp

//! Layer creation options of the GeoParquet writer, by their GDAL names where GDAL has them. They
//! are parsed in every build; writing needs the `parquet` feature (see [`crate::geoparquet`]).

use crate::{Error, Result};

/// The codec of the column chunks (GDAL's `COMPRESSION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParquetCompression {
    /// No compression (`NONE` or `UNCOMPRESSED`).
    Uncompressed,
    /// Snappy, the default of GDAL's Parquet driver and of terrio's.
    #[default]
    Snappy,
    /// Gzip, at a level from 0 to 9 (6 by default).
    Gzip,
    /// Brotli, at a level from 0 to 11 (1 by default).
    Brotli,
    /// LZ4 without framing (`LZ4_RAW`).
    Lz4Raw,
    /// Zstandard, at a level from 1 to 22 (1 by default). The level is honoured with the `libzstd`
    /// feature, in native builds. Without it, and in WebAssembly, pages are compressed with the
    /// pure-Rust `ruzstd` at its only level, about Zstandard's 1, whatever level is asked for (as
    /// terrio's ZSTD GeoTIFFs are), so files are larger than the zstd library's at higher levels.
    Zstd,
}

impl ParquetCompression {
    /// Parses a GDAL `COMPRESSION` name (`NONE`, `UNCOMPRESSED`, `SNAPPY`, `GZIP`, `BROTLI`,
    /// `LZ4_RAW`, `ZSTD`), ignoring case.
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "NONE" | "UNCOMPRESSED" => Self::Uncompressed,
            "SNAPPY" => Self::Snappy,
            "GZIP" => Self::Gzip,
            "BROTLI" => Self::Brotli,
            "LZ4_RAW" => Self::Lz4Raw,
            "ZSTD" => Self::Zstd,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unknown COMPRESSION '{other}' (NONE, UNCOMPRESSED, SNAPPY, GZIP, BROTLI, LZ4_RAW, ZSTD)"
                )));
            }
        })
    }

    /// The GDAL name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Uncompressed => "NONE",
            Self::Snappy => "SNAPPY",
            Self::Gzip => "GZIP",
            Self::Brotli => "BROTLI",
            Self::Lz4Raw => "LZ4_RAW",
            Self::Zstd => "ZSTD",
        }
    }

    /// The range of `COMPRESSION_LEVEL`, `None` for a codec without levels.
    pub fn levels(self) -> Option<std::ops::RangeInclusive<i32>> {
        match self {
            Self::Gzip => Some(0..=9),
            Self::Brotli => Some(0..=11),
            Self::Zstd => Some(1..=22),
            _ => None,
        }
    }

    /// An error unless `level` (if any) applies to the codec and is in its range.
    pub fn check_level(self, level: Option<i32>) -> Result<()> {
        match (level, self.levels()) {
            (None, _) => Ok(()),
            (Some(_), None) => Err(Error::InvalidArgument(format!("COMPRESSION_LEVEL does not apply to {}", self.name()))),
            (Some(l), Some(r)) if !r.contains(&l) => {
                Err(Error::InvalidArgument(format!("COMPRESSION_LEVEL of {} must be from {} to {}", self.name(), r.start(), r.end())))
            }
            _ => Ok(()),
        }
    }
}

/// The order of the rows within each layer (`SPATIAL_ORDER`, a terrio option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialOrder {
    /// The input order (`NONE`), written one batch at a time.
    #[default]
    None,
    /// The Hilbert order of the bounding-box centres over the layer's extent (`HILBERT`): the key
    /// of DuckDB spatial's `ST_Hilbert(x, y, box)` (see [`crate::geoparquet::hilbert_index`]).
    /// Ties keep the input order; null and empty geometries go last. A layer is buffered whole.
    Hilbert,
}

/// The columns with Parquet dictionary encoding (`DICTIONARY`, a terrio option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictionaryColumns {
    /// Every column (`ALL`), the `parquet` crate's default.
    #[default]
    All,
    /// Top-level string columns only (`STRING`), as pyarrow writes with `use_dictionary` naming
    /// them: a dictionary of unique numbers falls back to plain encoding in every row group and
    /// only adds a page.
    String,
    /// No column (`NONE`).
    None,
}

/// The granularity of the column statistics (`STATISTICS`, a terrio option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnStatistics {
    /// Column-chunk and page statistics with a page index (`PAGE`), binary minimum and maximum
    /// truncated to 64 bytes: the `parquet` crate's default.
    #[default]
    Page,
    /// Column-chunk statistics only (`CHUNK`), untruncated, without a page index, and none for
    /// all-null (`Null`-typed) columns: as pyarrow writes them.
    Chunk,
}

/// How to write GeoParquet files: the layer creation options of the GeoParquet writer, set by name
/// with [`GeoParquetWriteOptions::set`].
///
/// GDAL's Parquet driver (3.13) names `COMPRESSION`, `COMPRESSION_LEVEL`, `ROW_GROUP_SIZE`,
/// `GEOMETRY_NAME`, `WRITE_COVERING_BBOX` and `COVERING_BBOX_NAME`, with the same meanings and
/// defaults here. The others are terrio's: `LAYER_COLUMN` (several layers in one file),
/// `ROW_GROUP_PER_LAYER`, `SPATIAL_ORDER`, `HILBERT_COLUMN`, `DICTIONARY` and `STATISTICS`. GDAL's
/// `SORT_BY_BBOX=YES` (an R-tree order through a temporary GeoPackage) is not implemented and is
/// refused; `SPATIAL_ORDER=HILBERT` sorts instead.
///
/// ```
/// let mut options = terrio::GeoParquetWriteOptions::default();
/// for (k, v) in [("COMPRESSION", "ZSTD"), ("COMPRESSION_LEVEL", "3"), ("SPATIAL_ORDER", "HILBERT")] {
///     assert!(options.set(k, v)?);
/// }
/// assert!(!options.set("RFC7946", "YES")?);
/// # Ok::<(), terrio::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct GeoParquetWriteOptions {
    /// `GEOMETRY_NAME`: the WKB geometry column (`geometry`). For Arrow input, the input's
    /// geometry column too.
    pub geometry_column: String,
    /// `COMPRESSION`: the codec (`SNAPPY`).
    pub compression: ParquetCompression,
    /// `COMPRESSION_LEVEL`: the codec's level (its default when `None`). Checked against the codec
    /// when both are known and when writing.
    pub compression_level: Option<i32>,
    /// `ROW_GROUP_SIZE`: the most rows of a row group (65,536, GDAL's default).
    pub row_group_size: usize,
    /// `WRITE_COVERING_BBOX=YES|NO|AUTO`: whether to write the GeoParquet 1.1 bounding-box
    /// covering, a struct of `Float64` `xmin`, `ymin`, `xmax`, `ymax`, null for null and empty
    /// geometries (yes; `AUTO` is yes, as in GDAL without Parquet geometry types).
    pub write_covering_bbox: bool,
    /// `COVERING_BBOX_NAME`: the covering's column name (by default the geometry column's name
    /// with `_bbox` appended, as GDAL names it).
    pub covering_bbox_name: Option<String>,
    /// `LAYER_COLUMN`: a non-null string column, after the attributes, holding each layer's name,
    /// so that several layers share one file. Required to write more than one layer.
    pub layer_column: Option<String>,
    /// `ROW_GROUP_PER_LAYER=YES|NO`: whether a row group also ends at every layer boundary, so
    /// that a filter on the layer column skips whole row groups (yes).
    pub row_group_per_layer: bool,
    /// `SPATIAL_ORDER=NONE|HILBERT`: the order of the rows within each layer.
    pub spatial_order: SpatialOrder,
    /// `HILBERT_COLUMN`: with Hilbert order, a nullable `UInt64` column, after the covering, holding
    /// each row's key (null for null and empty geometries).
    pub hilbert_column: Option<String>,
    /// `DICTIONARY=ALL|STRING|NONE`: the columns with dictionary encoding.
    pub dictionary: DictionaryColumns,
    /// `STATISTICS=PAGE|CHUNK`: the column statistics.
    pub statistics: ColumnStatistics,
    /// The `crs` of the `geo` metadata (PROJJSON, or a CRS string GeoParquet readers accept)
    /// instead of the input's. Not a creation option: an argument of the bindings.
    pub crs: Option<serde_json::Value>,
}

impl Default for GeoParquetWriteOptions {
    fn default() -> Self {
        Self {
            geometry_column: "geometry".into(),
            compression: ParquetCompression::Snappy,
            compression_level: None,
            row_group_size: 65536,
            write_covering_bbox: true,
            covering_bbox_name: None,
            layer_column: None,
            row_group_per_layer: true,
            spatial_order: SpatialOrder::None,
            hilbert_column: None,
            dictionary: DictionaryColumns::All,
            statistics: ColumnStatistics::Page,
            crs: None,
        }
    }
}

/// A yes/no option value, as GDAL takes them.
fn yes_no(key: &str, value: &str) -> Result<bool> {
    match value.to_ascii_uppercase().as_str() {
        "YES" | "TRUE" | "ON" | "1" => Ok(true),
        "NO" | "FALSE" | "OFF" | "0" => Ok(false),
        _ => Err(Error::InvalidArgument(format!("{key} must be YES or NO, not '{value}'"))),
    }
}

/// A column name option value: empty for none.
fn name(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

impl GeoParquetWriteOptions {
    /// Sets an option by name, ignoring case (see the type's documentation). Returns `false` for a
    /// name it does not know, and an error for a value it refuses.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let key = key.to_ascii_uppercase();
        match key.as_str() {
            "GEOMETRY_NAME" => {
                if value.is_empty() {
                    return Err(Error::InvalidArgument("GEOMETRY_NAME needs a name".into()));
                }
                self.geometry_column = value.to_string();
            }
            "COMPRESSION" => {
                let codec = ParquetCompression::parse(value)?;
                codec.check_level(self.compression_level)?;
                self.compression = codec;
            }
            "COMPRESSION_LEVEL" => {
                let level =
                    value.trim().parse::<i32>().map_err(|_| Error::InvalidArgument("COMPRESSION_LEVEL must be an integer".into()))?;
                // The codec may be set after the level; it is checked then, and when writing.
                if self.compression.levels().is_some() {
                    self.compression.check_level(Some(level))?;
                }
                self.compression_level = Some(level);
            }
            "ROW_GROUP_SIZE" => {
                self.row_group_size = value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| Error::InvalidArgument("ROW_GROUP_SIZE must be a positive integer".into()))?
            }
            "WRITE_COVERING_BBOX" => self.write_covering_bbox = value.eq_ignore_ascii_case("AUTO") || yes_no(&key, value)?,
            "COVERING_BBOX_NAME" => self.covering_bbox_name = name(value),
            "LAYER_COLUMN" => self.layer_column = name(value),
            "ROW_GROUP_PER_LAYER" => self.row_group_per_layer = yes_no(&key, value)?,
            "SPATIAL_ORDER" => {
                self.spatial_order = match value.to_ascii_uppercase().as_str() {
                    "NONE" => SpatialOrder::None,
                    "HILBERT" => SpatialOrder::Hilbert,
                    _ => return Err(Error::InvalidArgument(format!("SPATIAL_ORDER must be NONE or HILBERT, not '{value}'"))),
                }
            }
            "SORT_BY_BBOX" => {
                if yes_no(&key, value)? {
                    return Err(Error::Unsupported(
                        "SORT_BY_BBOX=YES (GDAL's R-tree order through a temporary GeoPackage) is not implemented: use SPATIAL_ORDER=HILBERT"
                            .into(),
                    ));
                }
            }
            "HILBERT_COLUMN" => self.hilbert_column = name(value),
            "DICTIONARY" => {
                self.dictionary = match value.to_ascii_uppercase().as_str() {
                    "ALL" => DictionaryColumns::All,
                    "STRING" => DictionaryColumns::String,
                    "NONE" => DictionaryColumns::None,
                    _ => return Err(Error::InvalidArgument(format!("DICTIONARY must be ALL, STRING or NONE, not '{value}'"))),
                }
            }
            "STATISTICS" => {
                self.statistics = match value.to_ascii_uppercase().as_str() {
                    "PAGE" => ColumnStatistics::Page,
                    "CHUNK" => ColumnStatistics::Chunk,
                    _ => return Err(Error::InvalidArgument(format!("STATISTICS must be PAGE or CHUNK, not '{value}'"))),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Whether writing goes through [`crate::geoparquet`]: with any option set. With none, the
    /// default writer writes one layer and refuses several, as before.
    pub fn applies(&self) -> bool {
        *self != Self::default()
    }

    /// The covering's column name.
    pub fn covering_name(&self) -> String {
        self.covering_bbox_name.clone().unwrap_or_else(|| format!("{}_bbox", self.geometry_column))
    }
}
