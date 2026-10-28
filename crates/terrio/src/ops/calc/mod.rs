//! Raster algebra: an equivalent of `gdal_calc.py`.
//!
//! Expressions are evaluated band by band over named inputs with NumPy's typing rules (see
//! [`eval`]), as `gdal_calc.py` evaluates them with NumPy, so that results match it pixel for
//! pixel:
//!
//! - an output pixel is nodata when any input pixel is its band's nodata value (unless
//!   `hide_nodata`), and the result is blended with the output nodata value as `gdal_calc.py`
//!   blends it;
//! - the output type defaults to the smallest GDAL type that holds all the inputs' types, and
//!   the output nodata value to `gdal_calc.py`'s default for that type;
//! - results are converted to the output type with GDAL's rounding and clamping;
//! - inputs with different extents (but the same resolution) are aligned on the union or the
//!   intersection of their extents, or on a window, through [`mosaic`](super::mosaic).
//!
//! The expression language is the subset of Python and NumPy that raster algebra uses:
//! arithmetic (`+ - * / // % **`), comparisons (chainable), `&`, `|`, `^`, `~`, `and`, `or`,
//! `not`, and functions: `where`, `clip`, `minimum`/`maximum` (also `min`/`max` of two or more
//! values), `abs`, `sqrt`, `exp`, `log`, `log10`, `log2`, the trigonometric and hyperbolic
//! functions (`sin`, `arcsin` or `asin`, `arctan2` or `atan2`...), `floor`, `ceil`, `trunc`,
//! `round`, `isnan`, `isinf`, `isfinite`, `logical_and`/`or`/`xor`/`not`, `fmod`, `hypot`, type
//! conversions (`A.astype(float32)`, `float32(A)`) and the constants `pi`, `e`, `nan`, `inf`.
//! Functions may be prefixed with `numpy.` or `np.`. Nothing else is evaluated.

pub mod eval;
pub mod expr;

pub use eval::DType;
pub use expr::Expr;

use super::mosaic::{MosaicOptions, MosaicSource, mosaic};
use super::{Grid, build_raster};
use crate::{Error, RasterDataset, Result};
use eval::{Arr, Data, Evaluator, Val};
use std::borrow::Cow;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An input of [`calc`]: a band of a raster, known by a name in the expressions.
#[derive(Debug, Clone)]
pub struct CalcInput<'a> {
    /// The name (`A`, `B`... or any identifier).
    pub name: String,
    /// The raster.
    pub ds: &'a RasterDataset,
    /// The band, from 1.
    pub band: usize,
}

/// How inputs with different extents are handled (`--extent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CalcExtent {
    /// Inputs must have the same size; the output takes the georeferencing of the last input.
    #[default]
    Ignore,
    /// Inputs must have the same size and geotransform.
    Fail,
    /// The union of the inputs' extents, filled with nodata (or 0) outside each input.
    Union,
    /// The intersection of the inputs' extents.
    Intersect,
}

impl CalcExtent {
    /// Parses `ignore`, `fail`, `union` or `intersect`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "ignore" => Self::Ignore,
            "fail" => Self::Fail,
            "union" => Self::Union,
            "intersect" => Self::Intersect,
            other => return Err(Error::InvalidArgument(format!("unknown extent '{other}' (ignore, fail, union, intersect)"))),
        })
    }
}

/// The output nodata value (`--NoDataValue`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CalcNodata {
    /// `gdal_calc.py`'s default for the output type (255 for Byte, -32768 for Int16, the
    /// largest value for floating-point types...), or none with `hide_nodata`.
    #[default]
    Default,
    /// No nodata value.
    None,
    /// This value.
    Value(f64),
}

/// Options for [`calc`], named after `gdal_calc.py`'s.
#[derive(Debug, Clone, Default)]
pub struct CalcOptions {
    /// The expressions (`--calc`), one per output band (or one for all bands with `all_bands`).
    pub calc: Vec<String>,
    /// The output type (`--type`). By default the smallest type holding all the inputs' types.
    pub output_type: Option<DataType>,
    /// The output nodata value (`--NoDataValue`).
    pub nodata: CalcNodata,
    /// Ignore the inputs' nodata values (`--hideNoData`).
    pub hide_nodata: bool,
    /// How inputs with different extents are handled (`--extent`).
    pub extent: CalcExtent,
    /// Compute over this window `(ulx, uly, lrx, lry)`, aligned on the first input's pixels
    /// (`--projwin`).
    pub projwin: Option<(f64, f64, f64, f64)>,
    /// Process every band of this input, giving one output band each (`--allBands`).
    pub all_bands: Option<String>,
    /// Fail when the inputs' CRSs differ (`--projectionCheck`).
    pub projection_check: bool,
    /// [`calc_to_path`]: about how much memory (MiB) its strips of rows hold (256 by default).
    pub max_memory_mib: Option<f64>,
}

/// An input of [`calc_to_path`]: a band of a raster file, known by a name in the expressions.
#[derive(Debug, Clone)]
pub struct CalcPathInput {
    /// The name (`A`, `B`... or any identifier).
    pub name: String,
    /// The raster's path.
    pub path: String,
    /// The band, from 1.
    pub band: usize,
}

/// GDAL's `GDALDataTypeUnion()`: the smallest type holding both types.
pub fn data_type_union(a: DataType, b: DataType) -> DataType {
    let info = |d: DataType| -> (u32, bool, bool) {
        match d {
            DataType::U8 => (8, false, false),
            DataType::I8 => (8, true, false),
            DataType::U16 => (16, false, false),
            DataType::I16 => (16, true, false),
            DataType::U32 => (32, false, false),
            DataType::I32 => (32, true, false),
            DataType::U64 => (64, false, false),
            DataType::I64 => (64, true, false),
            DataType::F32 => (32, true, true),
            DataType::F64 => (64, true, true),
        }
    };
    let ((b0, s0, f0), (b1, s1, f1)) = (info(a), info(b));
    let bits = if f0 != f1 {
        let (fb, ib) = if f0 { (b0, b1) } else { (b1, b0) };
        fb.max(2 * ib)
    } else if s0 != s1 {
        if !s0 && b0 < b1 {
            b1
        } else if !s1 && b1 < b0 {
            b0
        } else {
            let (sb, ub) = if s0 { (b0, b1) } else { (b1, b0) };
            sb.max(2 * ub)
        }
    } else {
        b0.max(b1)
    };
    let (signed, floating) = (s0 || s1, f0 || f1);
    if floating {
        return if bits <= 32 { DataType::F32 } else { DataType::F64 };
    }
    match (bits, signed) {
        (..=8, false) => DataType::U8,
        (..=16, false) => DataType::U16,
        (..=32, false) => DataType::U32,
        (..=64, false) => DataType::U64,
        (..=8, true) => DataType::I8,
        (..=16, true) => DataType::I16,
        (..=32, true) => DataType::I32,
        (..=64, true) => DataType::I64,
        _ => DataType::F64,
    }
}

/// `gdal_calc.py`'s default output nodata value of a type, as the Python number it uses.
fn default_nodata(dt: DataType) -> Option<Val> {
    Some(match dt {
        DataType::U8 => Val::Int(255),
        DataType::U16 => Val::Int(65535),
        DataType::I16 => Val::Int(-32768),
        DataType::U32 => Val::Int(4294967293),
        DataType::I32 => Val::Int(-2147483647),
        DataType::F32 => Val::Float(3.402823466e38),
        DataType::F64 => Val::Float(f64::MAX),
        DataType::I8 | DataType::U64 | DataType::I64 => return None,
    })
}

fn dtype_of(dt: DataType) -> DType {
    match dt {
        DataType::U8 => DType::U8,
        DataType::I8 => DType::I8,
        DataType::U16 => DType::U16,
        DataType::I16 => DType::I16,
        DataType::U32 => DType::U32,
        DataType::I32 => DType::I32,
        DataType::U64 => DType::U64,
        DataType::I64 => DType::I64,
        DataType::F32 => DType::F32,
        DataType::F64 => DType::F64,
    }
}

/// Reads pixels `start..end` of a band as an array.
fn read_chunk(r: &wbraster::Raster, band: usize, start: usize, end: usize) -> Arr {
    let off = (band - 1) * r.rows * r.cols;
    values_of(&r.data, off + start, off + end)
}

/// Values `s..e` of raster data as an array of its type.
fn values_of(data: &RasterData, s: usize, e: usize) -> Arr {
    macro_rules! ints {
        ($v:expr) => {
            Data::I($v[s..e].iter().map(|x| *x as i128).collect())
        };
    }
    let dt = data.data_type();
    let data = match data {
        RasterData::U8(v) => ints!(v),
        RasterData::I8(v) => ints!(v),
        RasterData::U16(v) => ints!(v),
        RasterData::I16(v) => ints!(v),
        RasterData::U32(v) => ints!(v),
        RasterData::I32(v) => ints!(v),
        RasterData::U64(v) => ints!(v),
        RasterData::I64(v) => ints!(v),
        RasterData::F32(v) => Data::F(v[s..e].iter().map(|x| *x as f64).collect()),
        RasterData::F64(v) => Data::F(v[s..e].to_vec()),
    };
    Arr { dt: dtype_of(dt), data }
}

/// Converts an array to an output type as GDAL's `GDALCopyWords()` does (NumPy booleans are
/// written as bytes), returning each value as `f64`.
fn to_output(a: &Arr, out: DataType, n: usize) -> Vec<f64> {
    let int_out = !matches!(out, DataType::F32 | DataType::F64);
    let (lo, hi) = super::type_range(out);
    let unsigned = matches!(out, DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64);
    let conv = |i: usize| -> f64 {
        let i = if a.len() == 1 { 0 } else { i };
        match &a.data {
            Data::B(v) => v[i] as u8 as f64,
            Data::I(v) => {
                let x = v[i];
                if int_out {
                    (x.clamp(lo as i128, hi as i128)) as f64
                } else if out == DataType::F32 {
                    x as f32 as f64
                } else {
                    x as f64
                }
            }
            Data::F(v) => {
                let x = v[i];
                if !int_out {
                    return if out == DataType::F32 { x as f32 as f64 } else { x };
                }
                if x.is_nan() {
                    return 0.0;
                }
                let r = if a.dt == DType::F32 {
                    let x = x as f32;
                    (if unsigned || x > 0.0 { x + 0.5 } else { x - 0.5 }) as f64
                } else if unsigned || x > 0.0 {
                    x + 0.5
                } else {
                    x - 0.5
                };
                r.clamp(lo, hi).trunc()
            }
        }
    };
    (0..n).map(conv).collect()
}

/// Python's `%` on floats.
fn py_mod(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) { r + b } else { r }
}

/// `osgeo_utils`' `GeoRectangle`: `(min_x, min_y, width, height)`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn from_min_max(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        let (w, h) = (max_x - min_x, max_y - min_y);
        Self { x: min_x, y: min_y, w: if w <= 0.0 { 0.0 } else { w }, h: if h <= 0.0 { 0.0 } else { h } }
    }

    fn of(gt: &[f64; 6], w: usize, h: usize) -> Self {
        let (mut x, mut y, mut rw, mut rh) = (gt[0], gt[3], w as f64 * gt[1], h as f64 * gt[5]);
        if rw <= 0.0 {
            x += rw;
            rw = -rw;
        }
        if rh <= 0.0 {
            y += rh;
            rh = -rh;
        }
        Self { x, y, w: rw, h: rh }
    }

    fn max_x(&self) -> f64 {
        self.x + self.w
    }

    fn max_y(&self) -> f64 {
        self.y + self.h
    }

    fn union(&self, o: &Rect) -> Rect {
        Self::from_min_max(self.x.min(o.x), self.max_x().max(o.max_x()), self.y.min(o.y), self.max_y().max(o.max_y()))
    }

    fn intersect(&self, o: &Rect) -> Rect {
        Self::from_min_max(self.x.max(o.x), self.max_x().min(o.max_x()), self.y.max(o.y), self.max_y().min(o.max_y()))
    }

    /// The pixel-aligned rectangle around this one (`GeoRectangle.align`).
    fn align(&self, gt: &[f64; 6]) -> Rect {
        let left = self.x - py_mod(self.x - gt[0], gt[1]);
        let right = self.max_x() + (gt[1] - py_mod(self.max_x() - gt[0], gt[1]));
        let bottom = self.y + (gt[5] - py_mod(self.y - gt[3], gt[5]));
        let top = self.max_y() - py_mod(self.max_y() - gt[3], gt[5]);
        let (w, h) = (right - left, top - bottom);
        Rect { x: left, y: bottom, w: if w <= 0.0 { 0.0 } else { w }, h: if h <= 0.0 { 0.0 } else { h } }
    }
}

/// Evaluates expressions over input bands, as `gdal_calc.py` does.
pub fn calc(inputs: &[CalcInput], o: &CalcOptions) -> Result<RasterDataset> {
    let plan = plan(inputs, o)?;
    // Align inputs with different extents.
    let sources: Vec<Cow<RasterDataset>> = match &plan.align {
        None => inputs.iter().map(|a| Cow::Borrowed(a.ds)).collect(),
        Some(mo) => {
            inputs.iter().map(|a| mosaic(&[MosaicSource { path: a.name.clone(), ds: a.ds }], mo).map(Cow::Owned)).collect::<Result<_>>()?
        }
    };
    let in_nodata: Vec<Option<f64>> = sources.iter().map(|s| if o.hide_nodata { None } else { s.nodata() }).collect();

    // Evaluate in chunks of rows.
    let (width, height) = (plan.width, plan.height);
    let n = width * height;
    let chunk = chunk_len(width);
    let nchunks = n.div_ceil(chunk.max(1));
    let mut bands = Vec::with_capacity(plan.band_count);
    for b in 0..plan.band_count {
        let run = |c: usize| -> Result<Vec<f64>> {
            let (start, end) = (c * chunk, ((c + 1) * chunk).min(n));
            let arrays = inputs
                .iter()
                .zip(&sources)
                .enumerate()
                .map(|(i, (_, s))| read_chunk(&s.raster, plan.input_band(inputs, i, b), start, end))
                .collect();
            plan.eval_chunk(inputs, b, arrays, &in_nodata, end - start)
        };
        let chunks = run_chunks(nchunks, 16, &run)?;
        bands.push(chunks.into_iter().flatten().collect::<Vec<f64>>());
    }
    let grid = Grid { width, height, geo_transform: plan.geo_transform };
    build_raster(grid, bands, plan.out_type, plan.nodata(), &inputs[0].ds.crs(), |v, _| v)
}

/// The memory budget of [`calc_to_path`] when none is given.
const DEFAULT_MEMORY_MIB: f64 = 256.0;

/// [`calc`] over raster files, written to `path` a strip of rows at a time in about
/// `max_memory_mib` of memory, instead of reading every input whole. The output is the one
/// [`calc`] gives, value for value: inputs of different extents (`union`, `intersect`, a
/// `projwin`) are aligned as [`mosaic`] aligns them for [`calc`], with its rounding, and every
/// strip holds whole chunks of the rows [`calc`] evaluates at once.
///
/// An output that is also an input is computed whole first. Formats written whole (not GeoTIFF)
/// are buffered by the writer.
///
/// ```no_run
/// use terrio::ops::calc::{CalcOptions, CalcPathInput, calc_to_path};
///
/// let inputs = [CalcPathInput { name: "A".into(), path: "dem.tif".into(), band: 1 }];
/// let o = CalcOptions { calc: vec!["A * 0.3048".into()], max_memory_mib: Some(64.0), ..Default::default() };
/// calc_to_path(&inputs, &o, "dem_m.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn calc_to_path(inputs: &[CalcPathInput], o: &CalcOptions, path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
    if o.max_memory_mib.is_some_and(|m| m.is_nan() || m <= 0.0) {
        return Err(Error::InvalidArgument("max_memory_mib must be positive".into()));
    }
    let paths: Vec<String> = inputs.iter().map(|a| a.path.clone()).collect();
    let mut lazy = paths.iter().map(|p| crate::open_raster_lazy(p)).collect::<Result<Vec<_>>>()?;
    let mut read = paths.clone();
    for l in &lazy {
        read.extend(l.input_paths()?);
    }
    let headers: Vec<RasterDataset> = lazy.iter().map(|l| l.header()).collect();
    // An input that is also the output is read whole first.
    if super::strips::overwrites(path, &read) {
        drop(lazy);
        let full = paths.iter().map(|p| crate::open_raster(p)).collect::<Result<Vec<_>>>()?;
        let named: Vec<CalcInput> = inputs.iter().zip(&full).map(|(a, ds)| CalcInput { name: a.name.clone(), ds, band: a.band }).collect();
        return crate::write_raster_dataset(&calc(&named, o)?, path, options);
    }
    let named: Vec<CalcInput> = inputs.iter().zip(&headers).map(|(a, ds)| CalcInput { name: a.name.clone(), ds, band: a.band }).collect();
    let plan = plan(&named, o)?;
    let (w, h) = (plan.width, plan.height);

    // Each input's bands read: directly, or drawn on the inputs' common grid.
    let sources: Vec<[MosaicSource; 1]> = paths.iter().zip(&headers).map(|(p, ds)| [MosaicSource { path: p.clone(), ds }]).collect();
    let vrts = match &plan.align {
        Some(mo) => sources.iter().map(|s| super::mosaic::build_vrt(s, mo, None).map(Some)).collect::<Result<Vec<_>>>()?,
        None => vec![None; inputs.len()],
    };
    let bands: Vec<Vec<usize>> = (0..inputs.len())
        .map(|i| {
            let mut b: Vec<usize> = (0..plan.band_count).map(|k| plan.input_band(&named, i, k)).collect();
            b.dedup();
            b
        })
        .collect();
    let mut strips = Vec::with_capacity(inputs.len());
    let mut in_nodata = Vec::with_capacity(inputs.len());
    let mut value_bytes = 0;
    for (i, vrt) in vrts.iter().enumerate() {
        let nodata = match vrt {
            Some(v) => {
                let zero_based: Vec<usize> = bands[i].iter().map(|b| b - 1).collect();
                strips.push(Some(super::mosaic::Strips::new(v, &sources[i], &zero_based)?));
                value_bytes += bands[i].len() * v.bands[0].data_type.size_bytes();
                super::mosaic::mosaic_header(v)?.nodata()
            }
            None => {
                strips.push(None);
                value_bytes += bands[i].len() * headers[i].raster.data_type.size_bytes();
                headers[i].nodata()
            }
        };
        in_nodata.push(if o.hide_nodata { None } else { nodata });
    }

    // Strips of whole chunks, sized to the budget: about three strips of inputs (read ahead,
    // read, evaluated) and the output's values (as f64, then in the output type, queued).
    let chunk = chunk_len(w);
    let chunk_rows = (chunk / w.max(1)).max(1);
    let budget = (o.max_memory_mib.unwrap_or(DEFAULT_MEMORY_MIB) * 1048576.0) as usize;
    let out_bytes = plan.band_count * (8 + 2 * plan.out_type.size_bytes());
    let row = w * (3 * value_bytes + out_bytes);
    // The chunks evaluated at once hold their inputs and a few temporaries (16 bytes a value).
    let per_chunk = chunk * 16 * (inputs.len() + 4);
    let threads = (budget / 3 / per_chunk.max(1)).clamp(1, 16);
    let rows = ((budget / 2 / row.max(1)) / chunk_rows * chunk_rows).max(chunk_rows);

    let grid = Grid { width: w, height: h, geo_transform: plan.geo_transform };
    let header = super::raster_header(grid, plan.out_type, plan.band_count.max(1), plan.nodata(), &named[0].ds.crs())?;
    let mut out = super::strips::StripWriter::create(&header, path, options)?;
    let span = |s: usize| (s * rows, ((s + 1) * rows).min(h));
    super::strips::pipelined(
        h.div_ceil(rows),
        // Each input's bands over the strip, in its data type, one band after the other.
        |s| {
            let (y0, y1) = span(s);
            let mut data = Vec::with_capacity(inputs.len());
            for (i, l) in lazy.iter_mut().enumerate() {
                data.push(match &strips[i] {
                    Some(strip) => {
                        let reads = strip.read(std::slice::from_mut(l), y0..y1)?;
                        strip.draw(&reads, y0..y1)
                    }
                    None => {
                        let window = crate::Window::new(0, y0, w, y1 - y0);
                        l.read_window(window, &crate::WindowOptions { bands: bands[i].clone(), ..Default::default() })?.raster.data
                    }
                });
            }
            Ok(data)
        },
        |s, data| {
            let (y0, y1) = span(s);
            let n = w * (y1 - y0);
            let nchunks = n.div_ceil(chunk);
            let mut values = Vec::with_capacity(plan.band_count);
            for b in 0..plan.band_count {
                let run = |c: usize| -> Result<Vec<f64>> {
                    let (start, end) = (c * chunk, ((c + 1) * chunk).min(n));
                    let arrays = (0..inputs.len())
                        .map(|i| {
                            let k = bands[i].iter().position(|x| *x == plan.input_band(&named, i, b)).expect("band read");
                            values_of(&data[i], k * n + start, k * n + end)
                        })
                        .collect();
                    plan.eval_chunk(&named, b, arrays, &in_nodata, end - start)
                };
                values.push(run_chunks(nchunks, threads, &run)?.into_iter().flatten().collect::<Vec<f64>>());
            }
            drop(data);
            out.write(super::bands_data(&values, n, plan.out_type, |v, _| v), y1 - y0)
        },
    )?;
    out.finish()
}

/// The pixels of a chunk [`calc`] evaluates at once: whole rows of about 256 K pixels. Every
/// chunk is evaluated on its own, so a strip of whole chunks gives the values of the whole
/// raster.
fn chunk_len(width: usize) -> usize {
    (262_144 / width.max(1)).max(1) * width
}

/// What [`calc`] computes, checked and planned from the inputs' headers (their pixels are not
/// read).
struct Plan {
    exprs: Vec<Expr>,
    width: usize,
    height: usize,
    geo_transform: [f64; 6],
    out_type: DataType,
    out_nodata: Option<Val>,
    band_count: usize,
    all_bands_index: Option<usize>,
    /// The mosaic each input is aligned with (inputs of different extents, or a window).
    align: Option<MosaicOptions>,
}

impl Plan {
    /// The band of input `i` evaluated for output band `b`, from 1.
    fn input_band(&self, inputs: &[CalcInput], i: usize, b: usize) -> usize {
        if self.all_bands_index == Some(i) { b + 1 } else { inputs[i].band }
    }

    /// The output nodata value.
    fn nodata(&self) -> Option<f64> {
        self.out_nodata.as_ref().map(|v| match v {
            Val::Int(i) => *i as f64,
            // GDAL reads a Float32 nodata value this close to the largest float as that float.
            Val::Float(f) if self.out_type == DataType::F32 && (*f as f32).abs() == f32::MAX => *f as f32 as f64,
            Val::Float(f) => *f,
            Val::Arr(_) => unreachable!("Python number"),
        })
    }

    /// Output band `b` of a chunk of `len` pixels from the inputs' `arrays` (in the inputs'
    /// order), each input's pixels equal to its nodata value (`in_nodata`) masked.
    fn eval_chunk(&self, inputs: &[CalcInput], b: usize, arrays: Vec<Arr>, in_nodata: &[Option<f64>], len: usize) -> Result<Vec<f64>> {
        let expr = &self.exprs[if self.exprs.len() > 1 { b } else { 0 }];
        let mut mask: Option<Vec<bool>> = None;
        let mut vars: Vec<(String, Val)> = Vec::with_capacity(inputs.len());
        for (i, (a, arr)) in inputs.iter().zip(arrays).enumerate() {
            if let Some(nd) = in_nodata[i] {
                let eq = eval::binary_compare_eq(&Val::Arr(arr.clone()), &Val::Float(nd))?;
                let m = mask.get_or_insert_with(|| vec![false; len]);
                for (m, e) in m.iter_mut().zip(eq) {
                    *m |= e;
                }
            }
            vars.push((a.name.clone(), Val::Arr(arr)));
        }
        let lookup = |name: &str| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let mut result = Evaluator { lookup: &lookup }.eval(expr)?;
        let is_array = matches!(&result, Val::Arr(a) if a.len() == len);
        match (&mask, &self.out_nodata) {
            (Some(m), Some(nd)) => {
                let keep = Val::Arr(Arr { dt: DType::I64, data: Data::I(m.iter().map(|x| (!x) as i128).collect()) });
                let masked = Val::Arr(Arr { dt: DType::I64, data: Data::I(m.iter().map(|x| *x as i128).collect()) });
                let kept = eval::binary(expr::BinOp::Mul, &keep, &result)?;
                let filled = eval::binary(expr::BinOp::Mul, nd, &masked)?;
                result = eval::binary(expr::BinOp::Add, &kept, &filled)?;
            }
            _ if !is_array => {
                let ones = Val::Arr(Arr { dt: DType::F64, data: Data::F(vec![1.0; len]) });
                result = eval::binary(expr::BinOp::Mul, &ones, &result)?;
            }
            _ => {}
        }
        let arr = match result {
            Val::Arr(a) if a.dt == DType::F16 => a.cast(DType::F32),
            Val::Arr(a) => a,
            weak => eval::binary(expr::BinOp::Mul, &Val::Arr(Arr { dt: DType::F64, data: Data::F(vec![1.0; len]) }), &weak)?.into_arr(),
        };
        Ok(to_output(&arr, self.out_type, len))
    }
}

/// Checks the expressions and the inputs (their headers only) and plans [`calc`].
fn plan(inputs: &[CalcInput], o: &CalcOptions) -> Result<Plan> {
    if o.calc.is_empty() {
        return Err(Error::InvalidArgument("no calculation provided".into()));
    }
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("no input raster".into()));
    }
    let exprs = o.calc.iter().map(|c| Expr::parse(c)).collect::<Result<Vec<_>>>()?;
    for (i, a) in inputs.iter().enumerate() {
        if inputs[..i].iter().any(|b| b.name == a.name) {
            return Err(Error::InvalidArgument(format!("input '{}' is given twice", a.name)));
        }
        if a.band == 0 || a.band > a.ds.raster.bands {
            return Err(Error::InvalidArgument(format!("input '{}' has no band {}", a.name, a.band)));
        }
    }
    let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
    for e in &exprs {
        for n in e.names() {
            if !names.contains(&n.as_str())
                && DType::parse(&n).is_none()
                && !matches!(
                    n.as_str(),
                    "pi" | "e" | "nan" | "NaN" | "NAN" | "inf" | "Inf" | "Infinity" | "PINF" | "NINF" | "infty" | "euler_gamma"
                )
            {
                return Err(Error::InvalidArgument(format!("name '{n}' is not defined (inputs: {})", names.join(", "))));
            }
        }
    }

    // Check the inputs' sizes and georeferencing.
    let ignore = o.extent == CalcExtent::Ignore && o.projwin.is_none();
    let first = inputs[0].ds;
    let dims = (first.raster.cols, first.raster.rows);
    let mut gt_check = first.geo_transform();
    let mut differ = false;
    for a in inputs {
        let d = (a.ds.raster.cols, a.ds.raster.rows);
        if d != dims {
            differ = true;
            if matches!(o.extent, CalcExtent::Ignore | CalcExtent::Fail) && o.projwin.is_none() {
                return Err(Error::InvalidArgument(format!(
                    "the size of input '{}' ({}x{}) differs from the other inputs' ({}x{}): use an extent mode",
                    a.name, d.0, d.1, dims.0, dims.1
                )));
            }
        }
        if o.projection_check && !a.ds.crs().same_as(&first.crs()) {
            return Err(Error::InvalidArgument(format!("the CRS of input '{}' differs from the other inputs'", a.name)));
        }
        let gt = a.ds.geo_transform();
        if ignore {
            gt_check = gt;
        } else if gt != gt_check {
            let g0 = &first.geo_transform();
            let eps = 0.000001;
            let offset = eps >= (g0[0] - gt[0]).abs() + (g0[3] - gt[3]).abs();
            let pixel = eps >= (g0[1] - gt[1]).abs() + (g0[5] - gt[5]).abs();
            let rotation = eps >= (g0[2] - gt[2]).abs() + (g0[4] - gt[4]).abs();
            if !(offset && pixel && rotation) {
                differ = true;
                let reason = if !offset && o.extent == CalcExtent::Fail {
                    Some("different offset")
                } else if !pixel {
                    Some("different pixel size")
                } else if !rotation {
                    Some("different rotation")
                } else if g0[2] != 0.0 || g0[4] != 0.0 || gt[2] != 0.0 || gt[4] != 0.0 {
                    Some("non zero rotation")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(Error::InvalidArgument(format!("the geotransform of input '{}' is incompatible ({reason})", a.name)));
                }
            }
        }
    }

    // All bands of one input.
    let mut all_bands_index = None;
    let band_count = match &o.all_bands {
        Some(name) => {
            if exprs.len() > 1 {
                return Err(Error::InvalidArgument("all_bands implies a single expression".into()));
            }
            let i = inputs
                .iter()
                .position(|a| &a.name == name)
                .ok_or_else(|| Error::InvalidArgument(format!("all_bands: no input named '{name}'")))?;
            let n = inputs[i].ds.raster.bands;
            if n > 1 {
                all_bands_index = Some(i);
            }
            n
        }
        None => exprs.len(),
    };

    // Align inputs with different extents.
    let (mut width, mut height) = dims;
    let mut align = None;
    if !matches!(o.extent, CalcExtent::Fail) && !ignore && (differ || o.projwin.is_some()) {
        let rect = match o.projwin {
            Some((ulx, uly, lrx, lry)) => Rect::from_min_max(ulx, lrx, lry, uly).align(&first.geo_transform()),
            None => {
                let mut r: Option<Rect> = None;
                for a in inputs {
                    let e = Rect::of(&a.ds.geo_transform(), a.ds.raster.cols, a.ds.raster.rows);
                    r = Some(match r {
                        None => e,
                        Some(r) if o.extent == CalcExtent::Union => r.union(&e),
                        Some(r) => r.intersect(&e),
                    });
                }
                r.expect("inputs")
            }
        };
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return Err(Error::InvalidArgument("the requested extent is empty".into()));
        }
        let mo = MosaicOptions { extent: Some((rect.x, rect.y, rect.max_x(), rect.max_y())), ..Default::default() };
        // The grid of the first input's mosaic, which every input's shares.
        let vrt = super::mosaic::build_vrt(&[MosaicSource { path: inputs[0].name.clone(), ds: first }], &mo, None)?;
        (width, height) = (vrt.width, vrt.height);
        // As the mosaic's raster holds it.
        let grid = Grid { width, height, geo_transform: vrt.geo_transform.unwrap_or([0.0, 1.0, 0.0, 0.0, 0.0, -1.0]) };
        gt_check = super::raster_header(grid, first.raster.data_type, 1, None, &first.crs())?.geo_transform();
        align = Some(mo);
    }

    // The output type and nodata value.
    let out_type = match o.output_type {
        Some(t) => t,
        None => inputs.iter().map(|a| a.ds.raster.data_type).reduce(data_type_union).expect("inputs"),
    };
    let out_nodata: Option<Val> = match o.nodata {
        CalcNodata::Default if o.hide_nodata => None,
        CalcNodata::Default => default_nodata(out_type),
        CalcNodata::None => None,
        CalcNodata::Value(v) => Some(Val::Float(v)),
    };
    Ok(Plan { exprs, width, height, geo_transform: gt_check, out_type, out_nodata, band_count, all_bands_index, align })
}

/// Runs `f` on chunks `0..n`, in parallel (at most `max_threads` at once) where threads are
/// available.
fn run_chunks(n: usize, max_threads: usize, f: &(dyn Fn(usize) -> Result<Vec<f64>> + Sync)) -> Result<Vec<Vec<f64>>> {
    #[cfg(target_family = "wasm")]
    let _ = max_threads;
    #[cfg(not(target_family = "wasm"))]
    {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let threads = crate::par::threads().min(max_threads).min(n.max(1));
        if threads > 1 {
            let next = AtomicUsize::new(0);
            let results: Mutex<Vec<Option<Result<Vec<f64>>>>> = Mutex::new((0..n).map(|_| None).collect());
            std::thread::scope(|s| {
                for _ in 0..threads {
                    s.spawn(|| {
                        loop {
                            let c = next.fetch_add(1, Ordering::Relaxed);
                            if c >= n {
                                break;
                            }
                            let r = f(c);
                            results.lock().expect("no panic while holding the lock")[c] = Some(r);
                        }
                    });
                }
            });
            return results
                .into_inner()
                .expect("no panic while holding the lock")
                .into_iter()
                .map(|r| r.expect("every chunk ran"))
                .collect();
        }
    }
    (0..n).map(f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(values: Vec<f64>, w: usize, h: usize, dt: DataType, nodata: Option<f64>, x0: f64) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [x0, 1.0, 0.0, h as f64, 0.0, -1.0] };
        build_raster(grid, vec![values], dt, nodata, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    fn input<'a>(name: &str, ds: &'a RasterDataset) -> CalcInput<'a> {
        CalcInput { name: name.into(), ds, band: 1 }
    }

    #[test]
    fn ndvi_like() {
        let red = ds(vec![10.0, 20.0, 0.0, 40.0], 2, 2, DataType::U8, Some(0.0), 0.0);
        let nir = ds(vec![30.0, 20.0, 50.0, 200.0], 2, 2, DataType::U8, Some(0.0), 0.0);
        let o = CalcOptions {
            calc: vec!["(B.astype(float32) - A) / (B.astype(float32) + A)".into()],
            output_type: Some(DataType::F32),
            ..Default::default()
        };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.get(0, 0, 0), 0.5);
        assert_eq!(out.raster.get(0, 0, 1), 0.0);
        assert_eq!(out.raster.get(0, 1, 0), f32::MAX as f64);
        assert_eq!(out.nodata(), Some(f32::MAX as f64));
        // Byte arithmetic wraps, as in NumPy.
        let o = CalcOptions { calc: vec!["A + B".into()], ..Default::default() };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.data_type, DataType::U8);
        assert_eq!(out.raster.get(0, 1, 1), 240.0);
        assert_eq!(out.raster.get(0, 1, 0), 255.0);
        let o = CalcOptions { calc: vec!["A".into(), "B > 25".into()], hide_nodata: true, ..Default::default() };
        let out = calc(&[input("A", &red), input("B", &nir)], &o).unwrap();
        assert_eq!(out.raster.bands, 2);
        assert_eq!(out.nodata(), None);
        assert_eq!(out.raster.get(1, 0, 0), 1.0);
        assert_eq!(out.raster.get(1, 0, 1), 0.0);
    }

    #[test]
    fn extents() {
        let a = ds(vec![1.0; 4], 2, 2, DataType::I16, Some(-1.0), 0.0);
        let b = ds(vec![2.0; 4], 2, 2, DataType::I16, Some(-1.0), 1.0);
        let base = CalcOptions { calc: vec!["A + B".into()], ..Default::default() };
        assert!(calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Fail, ..base.clone() }).is_err());
        let u = calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Union, ..base.clone() }).unwrap();
        assert_eq!((u.raster.cols, u.raster.rows), (3, 2));
        assert_eq!(u.raster.get(0, 0, 1), 3.0);
        assert_eq!(u.raster.get(0, 0, 0), -32768.0);
        let i = calc(&[input("A", &a), input("B", &b)], &CalcOptions { extent: CalcExtent::Intersect, ..base.clone() }).unwrap();
        assert_eq!((i.raster.cols, i.raster.rows), (1, 2));
        assert_eq!(i.geo_transform()[0], 1.0);
        let c = ds(vec![2.0; 6], 3, 2, DataType::I16, None, 0.0);
        assert!(calc(&[input("A", &a), input("C", &c)], &base).is_err());
    }

    #[test]
    fn type_union() {
        assert_eq!(data_type_union(DataType::U8, DataType::I16), DataType::I16);
        assert_eq!(data_type_union(DataType::U16, DataType::I16), DataType::I32);
        assert_eq!(data_type_union(DataType::U8, DataType::F32), DataType::F32);
        assert_eq!(data_type_union(DataType::I32, DataType::F32), DataType::F64);
        assert_eq!(data_type_union(DataType::U32, DataType::I8), DataType::I64);
    }

    #[test]
    fn output_conversion() {
        let a = Arr { dt: DType::F64, data: Data::F(vec![2.5, -2.5, 300.0, f64::NAN, -0.4]) };
        assert_eq!(to_output(&a, DataType::U8, 5), vec![3.0, 0.0, 255.0, 0.0, 0.0]);
        assert_eq!(to_output(&a, DataType::I16, 5), vec![3.0, -3.0, 300.0, 0.0, 0.0]);
        assert_eq!(py_mod(-7.5, 2.0), 0.5);
        assert_eq!(py_mod(1.0, -3.0), -2.0);
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    /// A raster of `w` x `h` pixels of `bands` bands at `(x0, y_top)` with a 1-unit grid.
    #[allow(clippy::too_many_arguments)]
    fn raster(w: usize, h: usize, bands: usize, dt: DataType, nodata: Option<f64>, x0: f64, top: f64, seed: usize) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [x0, 1.0, 0.0, top, 0.0, -1.0] };
        let values = (0..bands)
            .map(|b| {
                (0..w * h)
                    .map(|i| {
                        let v = ((i * 7919 + seed * 104729 + b * 31) % 2003) as f64 - 400.0;
                        if (i + seed).is_multiple_of(37) {
                            nodata.unwrap_or(0.0)
                        } else if dt == DataType::F32 {
                            v * 0.37
                        } else {
                            v.abs() % 250.0
                        }
                    })
                    .collect()
            })
            .collect();
        build_raster(grid, values, dt, nodata, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    /// A raster's value bits, geotransform, nodata, band count and type.
    type Fingerprint = (Vec<u64>, [f64; 6], Option<f64>, usize, DataType);

    /// The bits of a raster's values and its georeferencing, to compare outputs exactly.
    fn fingerprint(ds: &RasterDataset) -> Fingerprint {
        (
            ds.raster.data.to_f64_vec().iter().map(|v| v.to_bits()).collect(),
            ds.geo_transform(),
            ds.nodata(),
            ds.raster.bands,
            ds.raster.data_type,
        )
    }

    /// Writes rasters, returning their paths.
    fn written(dir: &std::path::Path, rasters: &[&RasterDataset]) -> Vec<String> {
        rasters
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let p = dir.join(format!("in{i}.tif")).to_str().unwrap().to_string();
                crate::write_raster_dataset(r, &p, &Default::default()).unwrap();
                p
            })
            .collect()
    }

    /// `calc_to_path` gives `calc`'s output for matching, unioned and intersected extents, a
    /// window, mixed data types, every band of an input, and strips of one chunk or several.
    #[test]
    fn strips_give_the_whole_calculation() {
        let dir = tempfile::tempdir().unwrap();
        let a = raster(700, 900, 1, DataType::F32, Some(-9999.0), 0.0, 900.0, 1);
        let b = raster(700, 900, 3, DataType::I16, Some(-32768.0), 0.0, 900.0, 2);
        let c = raster(650, 820, 1, DataType::U8, Some(255.0), 30.0, 870.0, 3);
        // Off the others' grid by a fraction of a pixel: aligned by nearest neighbour.
        let d = raster(500, 600, 1, DataType::F32, None, -40.4, 950.7, 4);
        let paths = written(dir.path(), &[&a, &b, &c, &d]);
        let all = [&a, &b, &c, &d];
        // Each case: the inputs (raster, name, band) and the options.
        type Case<'a> = (Vec<(usize, &'a str, usize)>, CalcOptions);
        let cases: Vec<Case> = vec![
            (vec![(0, "A", 1), (1, "B", 2)], CalcOptions { calc: vec!["A * 2 + B".into()], ..Default::default() }),
            (
                vec![(0, "A", 1), (1, "B", 3)],
                CalcOptions {
                    calc: vec!["where(A > B, A, B)".into(), "B // 3".into()],
                    output_type: Some(DataType::I32),
                    ..Default::default()
                },
            ),
            (vec![(1, "B", 1)], CalcOptions { calc: vec!["B * 1.5".into()], all_bands: Some("B".into()), ..Default::default() }),
            (vec![(0, "A", 1), (2, "C", 1)], CalcOptions { calc: vec!["A + C".into()], extent: CalcExtent::Union, ..Default::default() }),
            (
                vec![(0, "A", 1), (2, "C", 1), (3, "D", 1)],
                CalcOptions { calc: vec!["A - C + D".into()], extent: CalcExtent::Intersect, hide_nodata: true, ..Default::default() },
            ),
            (
                vec![(2, "C", 1), (3, "D", 1)],
                CalcOptions {
                    calc: vec!["C * (D > 0)".into()],
                    extent: CalcExtent::Union,
                    nodata: CalcNodata::Value(-1.0),
                    ..Default::default()
                },
            ),
            (
                vec![(0, "A", 1), (1, "B", 1)],
                CalcOptions { calc: vec!["A / B".into()], projwin: Some((12.3, 700.2, 512.9, 101.0)), ..Default::default() },
            ),
        ];
        for (k, (named, o)) in cases.iter().enumerate() {
            let inputs: Vec<CalcInput> =
                named.iter().map(|(i, n, band)| CalcInput { name: n.to_string(), ds: all[*i], band: *band }).collect();
            let whole = fingerprint(&calc(&inputs, o).unwrap());
            let files: Vec<CalcPathInput> =
                named.iter().map(|(i, n, band)| CalcPathInput { name: n.to_string(), path: paths[*i].clone(), band: *band }).collect();
            // 0.01 MiB: strips of one chunk.
            for mib in [None, Some(0.01), Some(1.3)] {
                let out = dir.path().join(format!("out{k}.tif"));
                calc_to_path(&files, &CalcOptions { max_memory_mib: mib, ..o.clone() }, out.to_str().unwrap(), &Default::default())
                    .unwrap();
                let back = crate::open_raster(out.to_str().unwrap()).unwrap();
                assert!(fingerprint(&back) == whole, "case {k} ({:?}) at {mib:?} MiB", o.calc);
            }
        }
        // The same refusals as calc.
        let files = [
            CalcPathInput { name: "A".into(), path: paths[0].clone(), band: 1 },
            CalcPathInput { name: "C".into(), path: paths[2].clone(), band: 1 },
        ];
        let out = dir.path().join("x.tif");
        let o = CalcOptions { calc: vec!["A + C".into()], ..Default::default() };
        assert!(calc_to_path(&files, &o, out.to_str().unwrap(), &Default::default()).is_err());
        let o = CalcOptions { calc: vec!["A + C".into()], extent: CalcExtent::Union, max_memory_mib: Some(0.0), ..Default::default() };
        assert!(calc_to_path(&files, &o, out.to_str().unwrap(), &Default::default()).is_err());
    }

    /// An output that is also an input is computed whole first.
    #[test]
    fn an_input_overwritten_is_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let a = raster(300, 200, 1, DataType::F32, Some(-9999.0), 0.0, 200.0, 5);
        let paths = written(dir.path(), &[&a]);
        let o = CalcOptions { calc: vec!["A + 1".into()], max_memory_mib: Some(0.01), ..Default::default() };
        let whole = fingerprint(&calc(&[CalcInput { name: "A".into(), ds: &a, band: 1 }], &o).unwrap());
        calc_to_path(&[CalcPathInput { name: "A".into(), path: paths[0].clone(), band: 1 }], &o, &paths[0], &Default::default()).unwrap();
        assert!(fingerprint(&crate::open_raster(&paths[0]).unwrap()) == whole);
    }
}
