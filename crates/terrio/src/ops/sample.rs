//! Sample one or more real-valued raster bands at vector points, reprojecting their coordinates
//! for each raster. Sampling is nearest-neighbour (the pixel containing the point).

use crate::crs::{SpatialRef, Transformer};
use crate::{Error, FieldDef, FieldType, FieldValue, Geometry, Layer, RasterDataset, Result};

/// One raster band and the name of its output field in [`sample`].
pub struct SampleInput<'a> {
    /// Output field name. Collisions with existing or earlier sample fields get `_2`, `_3`, etc.
    pub field_name: String,
    /// Raster to sample. It is read into memory by the caller.
    pub raster: &'a RasterDataset,
    /// Band, from 1.
    pub band: usize,
}

/// Output controls for [`sample_with_options`]. Defaults retain input geometry and full precision.
#[derive(Debug, Clone, Default)]
pub struct SampleOptions {
    /// Reproject output geometries into this CRS after sampling. The input CRS must be known.
    pub dst_srs: Option<SpatialRef>,
    /// Round only newly sampled values to 0–15 decimal places. Ties round away from zero.
    /// Numeric fields remain floats; trailing zero display depends on the output format.
    pub decimal_places: Option<u8>,
}

/// Adds a Float field per input raster band, retaining the points' geometry (including Z/M),
/// attributes, IDs and CRS. Null geometries, nodata, non-finite pixels and out-of-bounds samples
/// yield null fields. A point that cannot be transformed into a raster's CRS also yields a null
/// sample for that raster. Failures to construct a CRS transformation remain errors, as do
/// non-point geometries and non-finite point XY coordinates.
/// Per-point failures record one warning per output field with their count (see
/// [`crate::warnings::take`]); the CLI prints it to standard error.
///
/// Rasters may have different affine grids and CRSs; points are transformed for each raster,
/// leaving their output geometries unchanged. Missing CRSs are allowed only when both point
/// and raster CRS are missing, in which case coordinates are used directly. Complex rasters
/// and geolocation arrays are unsupported. Pixel values are represented as f64.
///
/// ```no_run
/// use terrio::ops::sample::{sample, SampleInput};
/// let points = terrio::open_vector("sites.gpkg", &Default::default())?;
/// let dem = terrio::open_raster("dem.tif")?;
/// let inputs = [SampleInput { field_name: "elevation".into(), raster: &dem, band: 1 }];
/// let sampled = sample(&points.layers[0], &inputs)?;
/// terrio::write_vector(&[sampled], "sampled.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn sample(points: &Layer, inputs: &[SampleInput<'_>]) -> Result<Layer> {
    sample_with_options(points, inputs, &SampleOptions::default())
}

/// [`sample`] with optional output CRS and numeric rounding. Sampling always uses the original
/// points, so output reprojection cannot change the pixel selected. Output reprojection failures
/// are errors, even when sampling into an individual raster would have yielded null.
pub fn sample_with_options(points: &Layer, inputs: &[SampleInput<'_>], options: &SampleOptions) -> Result<Layer> {
    validate_decimal_places(options.decimal_places)?;
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("sampling needs at least one raster band".into()));
    }
    for f in &points.features {
        match &f.geometry {
            Some(Geometry::Point(c)) if c.x.is_finite() && c.y.is_finite() => {}
            None => {}
            Some(Geometry::Point(_)) => return Err(Error::InvalidArgument(format!("feature {} has non-finite XY coordinates", f.fid))),
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "feature {} is not a point; split multipart points with singlepart first",
                    f.fid
                )));
            }
        }
    }
    let src = SpatialRef::from_layer(points);
    let mut out = points.clone();
    for input in inputs {
        let ds = input.raster;
        check_raster(ds)?;
        check_band(ds, input.band)?;
        if input.field_name.trim().is_empty() {
            return Err(Error::InvalidArgument("sample field names cannot be empty".into()));
        }
        let dst = ds.crs();
        let ct = match (src.is_unknown(), dst.is_unknown()) {
            (true, true) => None,
            (true, false) | (false, true) => {
                return Err(Error::Crs("points and raster must both declare a CRS, or both have no CRS".into()));
            }
            _ if src.same_as(&dst) => None,
            _ => Some(Transformer::new(&src, &dst)?),
        };
        let gt = ds.geo_transform();
        let det = gt[1] * gt[5] - gt[2] * gt[4];
        let name = super::geometry::unique_field(&out, &input.field_name);
        let index = out.schema.len();
        out.schema.add_field(FieldDef::new(name.clone(), FieldType::Float));
        let mut projection_failures = 0usize;
        for f in &mut out.features {
            let value = match &f.geometry {
                Some(Geometry::Point(c)) => {
                    let coords = match &ct {
                        Some(t) => t.transform(c.x, c.y),
                        None => Ok((c.x, c.y)),
                    };
                    let (x, y) = match coords {
                        Ok(xy) => xy,
                        Err(_) => {
                            projection_failures += 1;
                            f.set_by_index(index, FieldValue::Null);
                            continue;
                        }
                    };
                    let (dx, dy) = (x - gt[0], y - gt[3]);
                    let col = ((gt[5] * dx - gt[2] * dy) / det).floor();
                    let row = ((gt[1] * dy - gt[4] * dx) / det).floor();
                    if col >= 0.0 && row >= 0.0 && col < ds.raster.cols as f64 && row < ds.raster.rows as f64 {
                        let v = ds
                            .raster
                            .data
                            .get_f64((input.band - 1) * ds.raster.rows * ds.raster.cols + row as usize * ds.raster.cols + col as usize);
                        if v.is_finite() && crate::histogram::is_valid(v, ds.nodata(), ds.raster.data_type) {
                            FieldValue::Float(v)
                        } else {
                            FieldValue::Null
                        }
                    } else {
                        FieldValue::Null
                    }
                }
                _ => FieldValue::Null,
            };
            f.attributes.resize(index, FieldValue::Null);
            f.attributes.push(value);
        }
        if projection_failures > 0 {
            let point_label = if projection_failures == 1 { "point" } else { "points" };
            crate::warnings::warn(format!(
                "sample field '{name}' in layer '{}': could not transform {projection_failures} {point_label} into the raster CRS; samples set to null",
                points.name
            ));
        }
    }
    finish_output(out, points.schema.len(), options.dst_srs.as_ref(), options.decimal_places)
}

/// One lazily read band and its output field. Handles may refer to local or remote sources.
pub struct LazySampleInput<'a> {
    /// Output field name, made unique as in [`SampleInput`].
    pub field_name: String,
    /// Raster opened with [`crate::open_raster_lazy`].
    pub raster: &'a mut crate::LazyRaster,
    /// Band number, from 1.
    pub band: usize,
}

/// Controls for [`sample_lazy`].
#[derive(Debug, Clone)]
pub struct LazySampleOptions {
    /// Output rounding and CRS, applied after all sampling.
    pub output: SampleOptions,
    /// Fail on the first per-point coordinate transformation error, rather than returning null.
    /// Construction of a transformation and raster I/O failures are always errors.
    pub strict_transform_errors: bool,
    /// Add a Text field named SAMPLE_FIELD + suffix, made unique, for each sample field.
    /// Values are `valid`, `nodata`, `outside`, `transform-error`, or `no-geometry`.
    /// `nodata` includes explicit masks and non-finite pixel values. None omits status fields.
    pub status_suffix: Option<String>,
    /// Maximum width and height of a returned pixel window; must be positive. Default: 512.
    /// Native block dimensions are used when smaller. Blocks larger than this limit may be
    /// decoded repeatedly if points occupy several subdivisions of a block.
    /// Decoder chunks and driver/network caches have their own limits; this does not bound them.
    pub window_size: usize,
}

impl Default for LazySampleOptions {
    fn default() -> Self {
        Self { output: SampleOptions::default(), strict_transform_errors: false, status_suffix: None, window_size: 512 }
    }
}

/// Sample full-resolution windows containing the input points, retaining one returned window
/// at a time. Points in the same window share a read, regardless of their original order.
/// Values, attributes, geometry and IDs remain in input order. The containing pixel is selected
/// by the inverse affine transform followed by floor, including on rotated and south-up grids.
/// Right and bottom pixel edges are outside. Coordinates use traditional GIS XY axis order.
///
/// Missing samples default to null, as in [`sample`]; optional status fields distinguish their
/// causes. Explicit internal/external dataset masks, per-band nodata (in the band's precision),
/// and non-finite pixels are invalid. Errors reading either pixels or masks are propagated.
/// Only affine, real-valued rasters are supported. Lazy-capable formats avoid materializing the
/// raster; a reader that falls back to memory at open time still holds its source in memory.
/// Indexing and output use memory proportional to the number of points, independently of raster
/// dimensions. Underlying codecs may decode a complete source tile/strip for a smaller window.
///
/// ```no_run
/// use terrio::ops::sample::{sample_lazy, LazySampleInput, LazySampleOptions};
/// let points = terrio::open_vector("sites.gpkg", &Default::default())?;
/// let mut dem = terrio::open_raster_lazy("dem.tif")?;
/// let mut inputs = [LazySampleInput { field_name: "elevation".into(), raster: &mut dem, band: 1 }];
/// let options = LazySampleOptions { status_suffix: Some("_status".into()), ..Default::default() };
/// let sampled = sample_lazy(&points.layers[0], &mut inputs, &options)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn sample_lazy(points: &Layer, inputs: &mut [LazySampleInput<'_>], options: &LazySampleOptions) -> Result<Layer> {
    use crate::{Window, WindowOptions};
    use std::collections::BTreeMap;

    validate_decimal_places(options.output.decimal_places)?;
    if inputs.is_empty() || options.window_size == 0 {
        return Err(Error::InvalidArgument("sampling needs a raster band and a positive window size".into()));
    }
    for f in &points.features {
        match &f.geometry {
            Some(Geometry::Point(c)) if c.x.is_finite() && c.y.is_finite() => {}
            None => {}
            _ => return Err(Error::InvalidArgument(format!("feature {} needs a point with finite XY coordinates", f.fid))),
        }
    }
    let src = SpatialRef::from_layer(points);
    let mut out = points.clone();
    for input in inputs {
        let ds = &mut *input.raster;
        let header = ds.header();
        check_raster(&header)?;
        check_band(&header, input.band)?;
        if input.field_name.trim().is_empty() {
            return Err(Error::InvalidArgument("sample field names cannot be empty".into()));
        }
        let dst = ds.crs();
        let ct = match (src.is_unknown(), dst.is_unknown()) {
            (true, true) => None,
            (true, false) | (false, true) => {
                return Err(Error::Crs("points and raster must both declare a CRS, or both have no CRS".into()));
            }
            _ if src.same_as(&dst) => None,
            _ => Some(Transformer::new(&src, &dst)?),
        };
        let name = super::geometry::unique_field(&out, &input.field_name);
        let index = out.schema.len();
        out.schema.add_field(FieldDef::new(name.clone(), FieldType::Float));
        let status_index = options.status_suffix.as_ref().map(|suffix| {
            let status_name = super::geometry::unique_field(&out, &format!("{name}{suffix}"));
            let i = out.schema.len();
            out.schema.add_field(FieldDef::new(status_name, FieldType::Text));
            i
        });
        for f in &mut out.features {
            f.attributes.resize(index, FieldValue::Null);
            f.attributes.push(FieldValue::Null);
            if status_index.is_some() {
                f.attributes.push(FieldValue::Text("no-geometry".into()));
            }
        }
        let gt = ds.geo_transform();
        let det = gt[1] * gt[5] - gt[2] * gt[4];
        let (bw, bh) = ds.block_size().unwrap_or((options.window_size, options.window_size));
        let width = bw.max(1).min(options.window_size);
        let height = bh.max(1).min(options.window_size);
        let mut windows = BTreeMap::<(usize, usize), Vec<(usize, usize, usize)>>::new();
        let mut failures = 0usize;
        for (i, f) in points.features.iter().enumerate() {
            let Some(Geometry::Point(c)) = &f.geometry else { continue };
            let xy = ct.as_ref().map_or(Ok((c.x, c.y)), |t| t.transform(c.x, c.y));
            let (x, y) = match xy {
                Ok((x, y)) if x.is_finite() && y.is_finite() => (x, y),
                other => {
                    if options.strict_transform_errors {
                        let detail = other.err().map_or_else(|| "non-finite transformed coordinates".into(), |e| e.to_string());
                        return Err(Error::Crs(format!("sample field '{name}', feature {}: {detail}", f.fid)));
                    }
                    failures += 1;
                    if let Some(j) = status_index {
                        out.features[i].attributes[j] = FieldValue::Text("transform-error".into());
                    }
                    continue;
                }
            };
            let (dx, dy) = (x - gt[0], y - gt[3]);
            let col = ((gt[5] * dx - gt[2] * dy) / det).floor();
            let row = ((gt[1] * dy - gt[4] * dx) / det).floor();
            if col >= 0.0 && row >= 0.0 && col < ds.width() as f64 && row < ds.height() as f64 {
                let (col, row) = (col as usize, row as usize);
                windows.entry((row / height, col / width)).or_default().push((i, col, row));
            } else if let Some(j) = status_index {
                out.features[i].attributes[j] = FieldValue::Text("outside".into());
            }
        }
        for ((by, bx), entries) in windows {
            let (x, y) = (bx * width, by * height);
            let window = Window::new(x, y, width.min(ds.width() - x), height.min(ds.height() - y));
            let pixels = ds.read_window(window, &WindowOptions { bands: vec![input.band], overview: None })?;
            let mask = ds.read_mask(window, None)?;
            let expected = window.width * window.height;
            if pixels.raster.data.len() != expected || mask.as_ref().is_some_and(|m| m.len() != expected) {
                return Err(Error::InvalidArgument("sample reader returned an unexpected window size".into()));
            }
            for (i, col, row) in entries {
                let offset = (row - y) * window.width + col - x;
                let v = pixels.raster.data.get_f64(offset);
                let valid = v.is_finite()
                    && crate::histogram::is_valid(v, ds.band_nodata(input.band), ds.band_data_type(input.band))
                    && mask.as_ref().is_none_or(|m| m[offset] != 0);
                if valid {
                    out.features[i].attributes[index] = FieldValue::Float(v);
                }
                if let Some(j) = status_index {
                    out.features[i].attributes[j] = FieldValue::Text(if valid { "valid" } else { "nodata" }.into());
                }
            }
        }
        if failures > 0 {
            crate::warnings::warn(format!(
                "sample field '{name}' in layer '{}': could not transform {failures} points into the raster CRS; samples set to null",
                points.name
            ));
        }
    }
    finish_output(out, points.schema.len(), options.output.dst_srs.as_ref(), options.output.decimal_places)
}

pub(crate) fn validate_decimal_places(decimal_places: Option<u8>) -> Result<()> {
    if decimal_places.is_some_and(|places| places > 15) {
        return Err(Error::InvalidArgument("decimal places must be between 0 and 15".into()));
    }
    Ok(())
}

// Shared by sampling and zonal statistics: preserve source attributes and integer counts,
// round only newly computed float fields, then transform output geometry independently.
pub(crate) fn finish_output(
    mut layer: Layer,
    first_field: usize,
    dst_srs: Option<&SpatialRef>,
    decimal_places: Option<u8>,
) -> Result<Layer> {
    if let Some(places) = decimal_places {
        let factor = 10_f64.powi(i32::from(places));
        for feature in &mut layer.features {
            for value in feature.attributes.iter_mut().skip(first_field) {
                if let FieldValue::Float(v) = value {
                    let scaled = *v * factor;
                    // Avoid a multiply/divide round trip when no fractional digits remain:
                    // it can perturb large or already rounded values by one floating-point step.
                    if scaled.is_finite() && scaled.fract() != 0.0 {
                        *v = scaled.round() / factor;
                    }
                }
            }
        }
    }
    match dst_srs {
        Some(dst) => crate::vector::reproject_layer_owned(layer, dst),
        None => Ok(layer),
    }
}

pub(crate) fn check_band(ds: &RasterDataset, band: usize) -> Result<()> {
    if band == 0 || band > ds.band_count() {
        return Err(Error::InvalidArgument(format!(
            "band {band} does not exist (bands are numbered from 1; the raster has {})",
            ds.band_count()
        )));
    }
    Ok(())
}

pub(crate) fn check_raster(ds: &RasterDataset) -> Result<()> {
    if ds.complex || ds.geolocation.is_some() {
        return Err(Error::Unsupported("this operation needs real-valued raster bands with affine georeferencing".into()));
    }
    let gt = ds.geo_transform();
    let det = gt[1] * gt[5] - gt[2] * gt[4];
    if gt.iter().any(|v| !v.is_finite()) || det == 0.0 || !det.is_finite() {
        return Err(Error::InvalidArgument("the raster geotransform must be finite and invertible".into()));
    }
    if ds.raster.cols == 0 || ds.raster.rows == 0 || ds.band_count() == 0 {
        return Err(Error::InvalidArgument("the raster must have pixels and at least one band".into()));
    }
    Ok(())
}

#[cfg(test)]
mod lazy_tests {
    use super::*;
    use crate::cog::WindowSource;
    use crate::{DataType, Feature, LazyRaster, Raster, RasterConfig, Window};
    use std::sync::{Arc, Mutex};
    use wbraster::raster::RasterData;

    type Reads = Arc<Mutex<Vec<(usize, Window, Vec<usize>)>>>;

    struct Source {
        reads: Reads,
        fail: bool,
    }
    impl WindowSource for Source {
        fn overviews(&self) -> Vec<(usize, usize)> {
            vec![(5000, 5000)]
        }
        fn block_size(&self) -> Option<(usize, usize)> {
            Some((4, 2))
        }
        fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
            self.reads.lock().unwrap().push((level, w, bands.to_vec()));
            if self.fail {
                return Err(Error::InvalidArgument("decoder failed".into()));
            }
            let values = (w.y..w.y + w.height).flat_map(|row| (w.x..w.x + w.width).map(move |col| (row * 10000 + col) as f32)).collect();
            Ok(RasterData::F32(values))
        }
        fn has_mask(&self) -> bool {
            true
        }
        fn read_mask(&mut self, level: usize, w: Window) -> Result<Option<Vec<u8>>> {
            assert_eq!(level, 0);
            assert!(w.width <= 4 && w.height <= 2);
            let mut mask = vec![255; w.width * w.height];
            mask[1] = 0;
            Ok(Some(mask))
        }
    }
    fn points(xy: &[(f64, f64)]) -> Layer {
        let mut l = Layer::new("points");
        l.features = xy
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| Feature { fid: i as u64 + 100, geometry: Some(Geometry::point(x, y)), attributes: vec![] })
            .collect();
        l
    }
    #[test]
    fn windows_are_bounded_grouped_full_resolution_and_scattered_in_input_order() {
        let reads = Arc::new(Mutex::new(vec![]));
        let mut header =
            RasterDataset::new(Raster::from_data(RasterConfig { cols: 1, rows: 1, ..Default::default() }, vec![0.0]).unwrap(), false);
        // Metadata only: a hundred million pixels, no hundred-million-element buffer.
        header.raster.cols = 10000;
        header.raster.rows = 10000;
        header.set_geo_transform([0., 1., 0., 0., 0., 1.]);
        let mut ds = LazyRaster::from_source(header.clone(), Box::new(Source { reads: reads.clone(), fail: false }));
        let mut p = points(&[(9., 3.), (0., 0.), (1., 0.), (9., 3.), (-0.1, 0.), (10000., 0.)]);
        p.features.push(Feature { fid: 999, geometry: None, attributes: vec![] });
        let options = LazySampleOptions { status_suffix: Some("_status".into()), ..Default::default() };
        let mut inputs = [LazySampleInput { field_name: "v".into(), band: 1, raster: &mut ds }];
        let out = sample_lazy(&p, &mut inputs, &options).unwrap();
        let expected = [
            (FieldValue::Float(30009.), "valid"),
            (FieldValue::Float(0.), "valid"),
            (FieldValue::Null, "nodata"),
            (FieldValue::Float(30009.), "valid"),
            (FieldValue::Null, "outside"),
            (FieldValue::Null, "outside"),
            (FieldValue::Null, "no-geometry"),
        ];
        for ((a, b), (v, status)) in out.features.iter().zip(&p.features).zip(expected) {
            assert_eq!(a.fid, b.fid);
            assert_eq!(a.geometry, b.geometry);
            assert_eq!(a.attributes, vec![v, FieldValue::Text(status.into())]);
        }
        let reads = reads.lock().unwrap();
        assert_eq!(reads.len(), 2, "one read per occupied native block, including repeated points");
        assert_eq!(reads.iter().map(|r| r.1.width * r.1.height).sum::<usize>(), 16);
        assert!(reads.iter().all(|(level, w, bands)| *level == 0 && w.width <= 4 && w.height <= 2 && bands == &[1]));
        let mut ds = LazyRaster::from_source(header, Box::new(Source { reads: Arc::default(), fail: true }));
        assert!(
            sample_lazy(&p, &mut [LazySampleInput { field_name: "v".into(), raster: &mut ds, band: 1 }], &options)
                .unwrap_err()
                .to_string()
                .contains("decoder failed")
        );
    }
    #[test]
    fn float32_nodata_precision_and_per_band_metadata() {
        let nodata = -3.4028234663852886e38;
        let config = RasterConfig { cols: 4, rows: 1, bands: 2, data_type: DataType::F32, nodata, ..Default::default() };
        let mut ds =
            RasterDataset::new(Raster::from_data(config, vec![nodata, 0., f64::NAN, 1., nodata, 0., f64::INFINITY, 1.]).unwrap(), true);
        ds.band_nodata = vec![Some(nodata), None];
        ds.set_geo_transform([0., 1., 0., 0., 0., 1.]);
        let mut ds = LazyRaster::from(ds);
        let p = points(&[(0., 0.), (1., 0.), (2., 0.), (3., 0.)]);
        let opts = LazySampleOptions { status_suffix: Some("_status".into()), ..Default::default() };
        let out = sample_lazy(&p, &mut [LazySampleInput { field_name: "v".into(), raster: &mut ds, band: 1 }], &opts).unwrap();
        assert_eq!(
            out.features.iter().map(|f| f.attributes[0].clone()).collect::<Vec<_>>(),
            vec![FieldValue::Null, FieldValue::Float(0.), FieldValue::Null, FieldValue::Float(1.)]
        );
        let out = sample_lazy(&p, &mut [LazySampleInput { field_name: "v".into(), raster: &mut ds, band: 2 }], &opts).unwrap();
        assert!(matches!(out.features[0].attributes[0],FieldValue::Float(v) if v==nodata as f32 as f64));
        assert_eq!(out.features[2].attributes[0], FieldValue::Null);
    }
}
