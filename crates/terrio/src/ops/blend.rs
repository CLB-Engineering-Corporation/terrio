//! Byte image composition compatible with GDAL's raster blend command.
//! Integer formulas ported from GDAL 3.13.3 (see NOTICE).
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use wbraster::{DataType, raster::RasterData};

/// Image composition operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendOperator {
    /// Overlay over the base image.
    #[default]
    SrcOver,
    /// Keep base hue/saturation and replace value.
    HsvValue,
    /// Multiply channels.
    Multiply,
    /// Screen channels.
    Screen,
    /// Overlay channels.
    Overlay,
    /// Hard light channels.
    HardLight,
    /// Darker channels.
    Darken,
    /// Lighter channels.
    Lighten,
    /// Color dodge.
    ColorDodge,
    /// Color burn.
    ColorBurn,
}
impl BlendOperator {
    /// Parses GDAL's operator names.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "src-over" => Self::SrcOver,
            "hsv-value" => Self::HsvValue,
            "multiply" => Self::Multiply,
            "screen" => Self::Screen,
            "overlay" => Self::Overlay,
            "hard-light" => Self::HardLight,
            "darken" => Self::Darken,
            "lighten" => Self::Lighten,
            "color-dodge" => Self::ColorDodge,
            "color-burn" => Self::ColorBurn,
            _ => return Err(Error::InvalidArgument(format!("unknown blend operator '{s}'"))),
        })
    }
}
/// Options of [`blend`].
#[derive(Debug, Clone)]
pub struct BlendOptions {
    /// Composition operator (default src-over).
    pub operator: BlendOperator,
    /// Overlay opacity, an integer percentage in 0..=100.
    pub opacity: u8,
}
impl Default for BlendOptions {
    fn default() -> Self {
        Self { operator: BlendOperator::SrcOver, opacity: 100 }
    }
}
fn mul(a: i32, b: i32) -> i32 {
    ((a as u8 as i32 * b as u8 as i32 + 255) / 256) as u8 as i32
}
// GDAL's generic kernels narrow intermediate arguments to Byte.
fn div(a: i32, b: i32) -> i32 {
    let a = a as u8 as i32;
    let b = b as u8 as i32;
    if a == 0 {
        0
    } else if b == 0 {
        255
    } else {
        (a * 255 / b) as u8 as i32
    }
}
fn component(op: BlendOperator, c: i32, a: i32, s: i32, sa: i32, alpha: i32) -> u8 {
    use BlendOperator::*;
    let outside = mul(c, 255 - sa) + mul(s, 255 - a);
    let cross = mul(a, sa);
    let value = match op {
        Multiply => mul(c, s) + outside,
        Screen => c + s - mul(c, s),
        Overlay | HardLight => {
            if 2 * c < a {
                2 * mul(c, s) + outside
            } else {
                cross - 2 * mul(a - c, sa - s) + outside
            }
        }
        Darken => mul(s, a).min(mul(c, sa)) + outside,
        Lighten => mul(s, a).max(mul(c, sa)) + outside,
        ColorDodge => {
            if mul(s, a) + mul(c, sa) >= cross {
                cross + outside
            } else {
                div(mul(c, sa), 255 - div(s, sa)) + outside
            }
        }
        ColorBurn => {
            if mul(s, a) + mul(c, sa) <= cross {
                outside
            } else {
                mul(cross, div(c, a) + div(s, sa) - 255) + outside
            }
        }
        _ => unreachable!(),
    };
    div(value, alpha) as u8
}
fn hsv(rgb: [i32; 3], v: i32) -> [u8; 3] {
    let [r, g, b] = rgb;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let saturation = delta as f32 / max.max(1) as f32;
    let denom = if delta == 0 { 1.0 } else { 6.0 * delta as f32 };
    let h = if max == b {
        4.0 / 6.0 + (r - g) as f32 / denom
    } else if max == g {
        2.0 / 6.0 + (b - r) as f32 / denom
    } else {
        let t = (g - b) as f32 / denom;
        if t < 0.0 { t + 1.0 } else { t }
    };
    let i = (6.0 * h) as usize;
    let f = 6.0 * h - i as f32;
    let p = (v as f32 * (1.0 - saturation) + 0.5) as u8;
    let q = (v as f32 * (1.0 - saturation * f) + 0.5) as u8;
    let t = (v as f32 * (1.0 - saturation * (1.0 - f)) + 0.5) as u8;
    let v = v as u8;
    [[v, t, p], [q, v, p], [p, v, t], [p, q, v], [t, p, v], [v, p, q]][i.min(5)]
}
fn rgba(ds: &RasterDataset, i: usize) -> [i32; 4] {
    let n = ds.raster.rows * ds.raster.cols;
    let get = |b| ds.raster.data.get_f64(b * n + i) as i32;
    if !ds.color_table.is_empty() && ds.raster.bands == 1 {
        let index = get(0);
        let mut rgba = ds.color_table.get(index as usize).copied().unwrap_or([0; 4]).map(i32::from);
        if ds.band_nodata(1) == Some(f64::from(index)) {
            rgba[3] = 0;
        }
        return rgba;
    }
    match ds.raster.bands {
        1 => [get(0), get(0), get(0), 255],
        2 => [get(0), get(0), get(0), get(1)],
        3 => [get(0), get(1), get(2), 255],
        _ => [get(0), get(1), get(2), get(3)],
    }
}
/// The inputs of a blend in the order they are composed, and the output's band count.
#[derive(Clone, Copy)]
struct Plan {
    operator: BlendOperator,
    /// The overlay opacity as a byte.
    opacity: i32,
    /// Whether the overlay is the base image (multiply, screen, overlay and hard-light when the
    /// overlay has more bands, as GDAL does).
    swapped: bool,
    /// The band counts of the base and source images, palettes expanded to RGBA.
    nb: usize,
    ns: usize,
}

/// Checks two inputs (their headers suffice) and plans their composition.
fn plan(color: &RasterDataset, overlay: &RasterDataset, o: &BlendOptions) -> Result<Plan> {
    use BlendOperator::*;
    for ds in [color, overlay] {
        if ds.complex || ds.raster.data_type != DataType::U8 || !(1..=4).contains(&ds.raster.bands) {
            return Err(Error::InvalidArgument("blend requires 1 to 4 Byte bands".into()));
        }
    }
    if o.opacity > 100 {
        return Err(Error::InvalidArgument("opacity must be in 0..=100".into()));
    }
    if (color.raster.cols, color.raster.rows) != (overlay.raster.cols, overlay.raster.rows) {
        return Err(Error::InvalidArgument("blend inputs must have the same dimensions".into()));
    }
    let bands = |ds: &RasterDataset| if ds.raster.bands == 1 && !ds.color_table.is_empty() { 4 } else { ds.raster.bands };
    let (nb, ns) = (bands(color), bands(overlay));
    if o.operator == HsvValue && nb < 3 {
        return Err(Error::InvalidArgument("hsv-value needs RGB or RGBA color input".into()));
    }
    if matches!(o.operator, Darken | Lighten) && nb.div_ceil(2) != ns.div_ceil(2) {
        return Err(Error::InvalidArgument("darken/lighten need equal color channel counts".into()));
    }
    let swapped = matches!(o.operator, Multiply | Screen | Overlay | HardLight) && nb < ns;
    let (nb, ns) = if swapped { (ns, nb) } else { (nb, ns) };
    Ok(Plan { operator: o.operator, opacity: (i32::from(o.opacity) * 255 + 50) / 100, swapped, nb, ns })
}

impl Plan {
    /// The base and source images among `color` and `overlay`.
    fn order<'a, T: ?Sized>(&self, color: &'a T, overlay: &'a T) -> (&'a T, &'a T) {
        if self.swapped { (overlay, color) } else { (color, overlay) }
    }

    /// The composed pixels of `base` and `source` (two images of the same size, or the same rows
    /// of two images), band after band.
    fn compose(&self, base: &RasterDataset, source: &RasterDataset) -> Vec<u8> {
        use BlendOperator::*;
        let Plan { operator, opacity, swapped, nb, ns } = *self;
        let n = base.raster.rows * base.raster.cols;
        let mut data = vec![0u8; n * nb];
        for i in 0..n {
            let mut b = rgba(base, i);
            let mut s = rgba(source, i);
            let out = if operator == HsvValue {
                let sa = if ns % 2 == 0 { mul(s[3], opacity) } else { opacity };
                let v = if ns < 3 { s[0] } else { s[0].max(s[1]).max(s[2]) };
                let bv = b[0].max(b[1]).max(b[2]);
                let rgb = hsv([b[0], b[1], b[2]], (v * sa + bv * (255 - sa) + 255) / 256);
                [rgb[0], rgb[1], rgb[2], b[3] as u8]
            } else if operator == SrcOver {
                if nb < 3 && ns >= 3 {
                    s[0] = (s[0] * 306 + s[1] * 601 + s[2] * 117) / 1024;
                    s[1] = s[0];
                    s[2] = s[0];
                }
                let sa = if ns % 2 == 0 { mul(s[3], opacity) } else { opacity };
                let ba = mul(b[3], 255 - sa);
                let alpha = sa + ba;
                let mut out = [0u8; 4];
                for j in 0..3 {
                    let mut v = (s[j] * sa + b[j] * ba + 255) / 256;
                    if alpha != 0 && alpha != 255 {
                        v = (v * 255 + alpha / 2) / alpha;
                    }
                    out[j] = v.min(255) as u8;
                }
                out[3] = alpha as u8;
                out
            } else {
                if swapped {
                    b[3] = mul(b[3], opacity);
                } else {
                    s[3] = mul(s[3], opacity);
                }
                let a = b[3];
                let sa = s[3];
                let alpha = sa + a - mul(sa, a);
                for j in 0..3 {
                    b[j] = mul(b[j], a);
                    s[j] = mul(s[j], sa);
                }
                if operator == HardLight {
                    std::mem::swap(&mut b, &mut s);
                }
                let mut out = [0u8; 4];
                for j in 0..3 {
                    out[j] = component(operator, b[j], b[3], s[j], s[3], alpha);
                }
                out[3] = alpha as u8;
                out
            };
            for j in 0..nb {
                data[j * n + i] = out[if nb == 2 && j == 1 { 3 } else { j }];
            }
        }
        data
    }

    /// The output of blending onto `base` (a header suffices), without pixels: georeferenced as
    /// `base`, with color interpretations but no nodata or metadata.
    fn header(&self, base: &RasterDataset) -> RasterDataset {
        let nb = self.nb;
        let mut out = super::derived_header(base, DataType::U8, nb);
        out.raster.metadata.clear();
        out.has_nodata = false;
        out.band_metadata = (0..nb)
            .map(|j| crate::dataset::BandMetadata {
                color_interp: Some(
                    (if nb <= 2 { if j == 0 { "Gray" } else { "Alpha" } } else { ["Red", "Green", "Blue", "Alpha"][j] }).into(),
                ),
                ..Default::default()
            })
            .collect();
        out
    }
}

/// Blends equal-sized Byte images by pixel position, without reprojection.
/// One/two bands mean gray/gray-alpha; three/four mean RGB/RGBA. Palettes are
/// expanded to RGBA. Nodata and separate validity masks are ignored, as in GDAL;
/// transparency comes from alpha (palette nodata becomes transparent on expansion).
/// Output georeferencing follows the color input
/// (or the swapped RGB input for multiply/screen/overlay/hard-light).
/// Inputs and output are materialized; no extra full-image working buffers are used.
/// [`blend_to_path`] writes the same pixels without holding the images whole.
pub fn blend(color: &RasterDataset, overlay: &RasterDataset, o: &BlendOptions) -> Result<RasterDataset> {
    let plan = plan(color, overlay, o)?;
    let (base, source) = plan.order(color, overlay);
    let mut out = plan.header(base);
    out.raster.data = RasterData::U8(plan.compose(base, source));
    Ok(out)
}

/// Blends two equal-sized Byte images as [`blend`] does and writes the result to `path`, reading
/// both inputs and writing a GeoTIFF or COG a strip of rows at a time, so that no image is held
/// whole (other output formats are gathered and written at the end). The pixels, georeferencing
/// and color interpretations are those of [`blend`].
///
/// `path` must not be a file that `color` or `overlay` reads, which writing it would truncate:
/// read such an input whole with [`crate::open_raster`] and use [`blend`].
///
/// ```no_run
/// use terrio::ops::blend::{BlendOperator, BlendOptions, blend_to_path};
///
/// let mut relief = terrio::open_raster_lazy("relief.tif")?;
/// let mut shade = terrio::open_raster_lazy("hillshade.tif")?;
/// let o = BlendOptions { operator: BlendOperator::HsvValue, opacity: 60 };
/// blend_to_path(&mut relief, &mut shade, &o, "shaded.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn blend_to_path(
    color: &mut LazyRaster,
    overlay: &mut LazyRaster,
    o: &BlendOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    let (hc, ho) = (color.header(), overlay.header());
    let p = plan(&hc, &ho, o)?;
    // Per pixel: both inputs and the output, a byte per band.
    let bands = hc.raster.bands + ho.raster.bands + p.nb;
    // Strips of whole rows of the base image's blocks.
    let block = if p.swapped { overlay.block_size() } else { color.block_size() }.map(|b| b.1);
    let rows = super::strips::strip_rows(hc.raster.cols, bands, 1, block);
    blend_in_strips(color, overlay, p, path, options, rows)
}

/// [`blend_to_path`] in strips of `rows` rows.
fn blend_in_strips(
    color: &mut LazyRaster,
    overlay: &mut LazyRaster,
    p: Plan,
    path: &str,
    options: &crate::RasterWriteOptions,
    rows: usize,
) -> Result<()> {
    let (hc, ho) = (color.header(), overlay.header());
    let (w, h) = (hc.raster.cols, hc.raster.rows);
    let mut out = super::strips::StripWriter::create(&p.header(p.order(&hc, &ho).0), path, options)?;
    let read = WindowOptions::default();
    super::strips::pipelined(
        h.div_ceil(rows),
        |i| {
            let (y0, y1) = (i * rows, ((i + 1) * rows).min(h));
            let window = Window::new(0, y0, w, y1 - y0);
            Ok((color.read_window(window, &read)?, overlay.read_window(window, &read)?))
        },
        |_, (c, v)| {
            let (base, source) = p.order(&c, &v);
            out.write(RasterData::U8(p.compose(base, source)), c.raster.rows)
        },
    )?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(values: &[u8]) -> RasterDataset {
        super::super::build_raster(
            super::super::Grid { width: 1, height: 1, geo_transform: [10., 1., 0., 20., 0., -1.] },
            values.iter().map(|v| vec![f64::from(*v)]).collect(),
            DataType::U8,
            None,
            &crate::crs::SpatialRef::default(),
            super::super::convert_value,
        )
        .unwrap()
    }
    #[test]
    fn operators_match_frozen_gdal_3_13_3_rgb_values() {
        for (name, expected) in [
            ("src-over", [250., 20., 100.]),
            ("hsv-value", [44., 118., 250.]),
            ("multiply", [30., 7., 67.]),
            ("screen", [250., 93., 203.]),
            ("overlay", [60., 14., 151.]),
            ("hard-light", [245., 14., 134.]),
            ("darken", [30., 20., 100.]),
            ("lighten", [250., 80., 170.]),
            ("color-dodge", [255., 86., 255.]),
            ("color-burn", [25., 0., 15.]),
        ] {
            let out = blend(
                &image(&[30, 80, 170]),
                &image(&[250, 20, 100]),
                &BlendOptions { operator: BlendOperator::parse(name).unwrap(), opacity: 100 },
            )
            .unwrap();
            assert_eq!((0..3).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), expected, "{name}");
        }
    }

    #[test]
    fn palette_survives_tiff_lazy_and_memory_reads() {
        let file = tempfile::Builder::new().suffix(".tif").tempfile().unwrap();
        let source = image(&[1]);
        let palette = vec![[0, 0, 0, 255], [30, 80, 170, 255]];
        crate::write_raster_dataset(
            &source,
            file.path().to_str().unwrap(),
            &crate::RasterWriteOptions { color_table: palette.clone(), ..Default::default() },
        )
        .unwrap();
        let bytes = std::fs::read(file.path()).unwrap();
        let mut lazy = crate::open_raster_lazy(file.path().to_str().unwrap()).unwrap();
        assert_eq!(&lazy.header().color_table[..2], &palette);
        let window = lazy.read_window(crate::Window::new(0, 0, 1, 1), &Default::default()).unwrap();
        assert_eq!(&window.color_table[..2], &palette);
        let read = crate::memory::open_raster_bytes(bytes, "palette.tif").unwrap();
        assert_eq!(&read.color_table[..2], &palette);
        let out = blend(&read, &image(&[0, 0, 0, 0]), &Default::default()).unwrap();
        assert_eq!((0..4).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), vec![30., 80., 170., 255.]);
    }

    #[test]
    fn palette_writes_inherit_consistently_and_allow_grayscale_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.tif");
        let input = input.to_str().unwrap();
        let source = super::super::build_raster(
            super::super::Grid { width: 4, height: 6, geo_transform: [10., 1., 0., 20., 0., -1.] },
            vec![(0..24).map(|i| (i % 2) as f64).collect()],
            DataType::U8,
            None,
            &crate::crs::SpatialRef::default(),
            super::super::convert_value,
        )
        .unwrap();
        let palette = vec![[0, 0, 0, 255], [30, 80, 170, 255]];
        crate::write_raster_dataset(&source, input, &crate::RasterWriteOptions { color_table: palette.clone(), ..Default::default() })
            .unwrap();
        let source = crate::open_raster(input).unwrap();
        for (case, mut options, keeps_palette) in [
            ("plain", crate::RasterWriteOptions::default(), true),
            ("cog", crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), ..Default::default() }, true),
            ("jpeg", crate::RasterWriteOptions { compression: Some("JPEG".into()), ..Default::default() }, false),
            (
                "jpeg-co",
                crate::RasterWriteOptions { creation_options: vec![("COMPRESS".into(), "JPEG".into())], ..Default::default() },
                false,
            ),
            (
                "gray",
                crate::RasterWriteOptions { creation_options: vec![("PHOTOMETRIC".into(), "MINISBLACK".into())], ..Default::default() },
                false,
            ),
            (
                "palette",
                crate::RasterWriteOptions { creation_options: vec![("PHOTOMETRIC".into(), "PALETTE".into())], ..Default::default() },
                true,
            ),
        ] {
            for lazy in [false, true] {
                let file = dir.path().join(format!("{case}-{lazy}.tif"));
                let path = file.to_str().unwrap();
                if lazy {
                    crate::open_raster_lazy(input).unwrap().write(path, &options).unwrap();
                } else {
                    crate::write_raster_dataset(&source, path, &options).unwrap();
                }
                let reader = crate::open_raster_lazy(path).unwrap();
                let header = reader.header();
                assert_eq!(header.color_table.is_empty(), !keeps_palette, "{case}, lazy={lazy}");
                if keeps_palette {
                    assert_eq!(&header.color_table[..2], &palette);
                }
                assert_eq!(header.band_color_interp(1), if keeps_palette { "Palette" } else { "Gray" });
            }
            let encoded = crate::memory::codec::write_raster(
                &source,
                options.driver.unwrap_or(crate::formats::RasterDriver::GTiff),
                &options,
                "palette",
            )
            .unwrap();
            let header = crate::LazyRaster::from_tiff_bytes(encoded).unwrap().unwrap().header();
            assert_eq!(header.color_table.is_empty(), !keeps_palette, "memory {case}");
            assert_eq!(header.band_color_interp(1), if keeps_palette { "Palette" } else { "Gray" });
            if case == "jpeg" {
                options.color_table = palette.clone();
                assert!(crate::write_raster_dataset(&source, dir.path().join("explicit-invalid.tif").to_str().unwrap(), &options).is_err());
            }
        }
        let file = dir.path().join("rows.tif");
        let mut writer = crate::GeoTiffWriter::create(file.to_str().unwrap(), &source, &Default::default()).unwrap();
        writer.write_rows(&source.raster.data, 6, None).unwrap();
        writer.finish().unwrap();
        assert_eq!(&crate::open_raster_lazy(file.to_str().unwrap()).unwrap().header().color_table[..2], &palette);
    }

    #[test]
    fn alpha_and_palette_are_not_nodata() {
        let mut base = image(&[30, 80, 170, 255]);
        base.has_nodata = true;
        base.raster.nodata = 30.;
        let transparent = image(&[250, 20, 0, 0]);
        let out = blend(&base, &transparent, &Default::default()).unwrap();
        assert_eq!((0..4).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), vec![30., 80., 170., 255.]);
        assert_eq!(out.nodata(), None);
        let mut palette = image(&[1]);
        palette.color_table = vec![[0, 0, 0, 0], [30, 80, 170, 255]];
        assert_eq!(blend(&palette, &transparent, &Default::default()).unwrap().raster.bands, 4);
        assert!(blend(&base, &transparent, &BlendOptions { opacity: 101, ..Default::default() }).is_err());
        assert!(blend(&image(&[1]), &transparent, &BlendOptions { operator: BlendOperator::HsvValue, ..Default::default() }).is_err());
        assert!(BlendOperator::parse("bogus").is_err());
    }

    /// A `bands`-band Byte image of `w` x `h` pixels with varied values and alpha, written to
    /// `path` (tiled with 16-pixel tiles when `tiled`), with a palette when given.
    fn write_image(path: &str, w: usize, h: usize, bands: usize, seed: usize, tiled: bool, palette: Vec<[u8; 4]>) {
        let n = w * h;
        let values = (0..bands)
            .map(|b| {
                (0..n)
                    .map(|i| {
                        let v = (i * (37 + 11 * b) + seed * 101 + (i / w) * 7) % 256;
                        // Alpha bands hold 0 and 255 as well as partial values.
                        f64::from(if bands.is_multiple_of(2) && b == bands - 1 { [0, 255, v as u8, 255][i % 4] } else { v as u8 })
                    })
                    .collect()
            })
            .collect();
        let ds = super::super::build_raster(
            super::super::Grid { width: w, height: h, geo_transform: [500000., 2., 0., 4500000., 0., -2.] },
            values,
            DataType::U8,
            None,
            &crate::crs::SpatialRef::parse("EPSG:32615").unwrap(),
            super::super::convert_value,
        )
        .unwrap();
        let options = crate::RasterWriteOptions { tile_size: tiled.then_some(16), color_table: palette, ..Default::default() };
        crate::write_raster_dataset(&ds, path, &options).unwrap();
    }

    /// Asserts that two rasters have the same pixels, georeferencing and band descriptions.
    fn assert_same_raster(a: &str, b: &str, what: &str) {
        let (a, b) = (crate::open_raster(a).unwrap(), crate::open_raster(b).unwrap());
        assert_eq!(a.raster.data.to_f64_vec(), b.raster.data.to_f64_vec(), "{what}");
        assert_eq!(a.raster.data_type, b.raster.data_type, "{what}");
        assert_eq!(a.geo_transform(), b.geo_transform(), "{what}");
        assert!(a.crs() == b.crs(), "{what}");
        assert_eq!(a.band_metadata, b.band_metadata, "{what}");
        assert_eq!((a.nodata(), &a.raster.metadata, &a.color_table), (b.nodata(), &b.raster.metadata, &b.color_table), "{what}");
    }

    #[test]
    fn streamed_blend_writes_the_in_memory_blend() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let (w, h) = (37, 29);
        let palette: Vec<[u8; 4]> = (0..256).map(|i| [i as u8, (255 - i) as u8, (i * 3) as u8, if i % 5 == 0 { 0 } else { 255 }]).collect();
        // (color bands, overlay bands, color palette): gray, gray-alpha, RGB, RGBA and a palette,
        // including overlays with more bands than the color input (swapped for multiply...).
        let layouts = [(4, 1, false), (3, 2, false), (1, 4, false), (2, 3, false), (1, 4, true), (4, 4, false)];
        for (k, (nc, no, pal)) in layouts.into_iter().enumerate() {
            let (c, v) = (path(&format!("c{k}.tif")), path(&format!("v{k}.tif")));
            write_image(&c, w, h, nc, k, k % 2 == 0, if pal { palette.clone() } else { Vec::new() });
            write_image(&v, w, h, no, k + 7, k % 2 == 1, Vec::new());
            let (cd, vd) = (crate::open_raster(&c).unwrap(), crate::open_raster(&v).unwrap());
            for name in
                ["src-over", "hsv-value", "multiply", "screen", "overlay", "hard-light", "darken", "lighten", "color-dodge", "color-burn"]
            {
                for opacity in [100, 37] {
                    let o = BlendOptions { operator: BlendOperator::parse(name).unwrap(), opacity };
                    let Ok(expected) = blend(&cd, &vd, &o) else {
                        // Refused alike, from the headers alone.
                        let (mut cl, mut vl) = (crate::open_raster_lazy(&c).unwrap(), crate::open_raster_lazy(&v).unwrap());
                        assert!(blend_to_path(&mut cl, &mut vl, &o, &path("refused.tif"), &Default::default()).is_err(), "{k} {name}");
                        assert!(!std::path::Path::new(&path("refused.tif")).exists());
                        continue;
                    };
                    crate::write_raster_dataset(&expected, &path("whole.tif"), &Default::default()).unwrap();
                    for rows in [1, 7, h] {
                        let (mut cl, mut vl) = (crate::open_raster_lazy(&c).unwrap(), crate::open_raster_lazy(&v).unwrap());
                        let p = plan(&cl.header(), &vl.header(), &o).unwrap();
                        let out = path("strips.tif");
                        blend_in_strips(&mut cl, &mut vl, p, &out, &Default::default(), rows).unwrap();
                        assert_same_raster(&out, &path("whole.tif"), &format!("{k} {name} {opacity}% in strips of {rows}"));
                    }
                    // The public entry point, to a COG.
                    let cog = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), ..Default::default() };
                    let (mut cl, mut vl) = (crate::open_raster_lazy(&c).unwrap(), crate::open_raster_lazy(&v).unwrap());
                    blend_to_path(&mut cl, &mut vl, &o, &path("streamed.cog.tif"), &cog).unwrap();
                    crate::write_raster_dataset(&expected, &path("whole.cog.tif"), &cog).unwrap();
                    assert!(
                        std::fs::read(path("streamed.cog.tif")).unwrap() == std::fs::read(path("whole.cog.tif")).unwrap(),
                        "{k} {name} COG"
                    );
                }
            }
        }
        // Inputs of different sizes are refused before anything is written.
        write_image(&path("small.tif"), w - 1, h, 3, 0, false, Vec::new());
        let (mut cl, mut vl) = (crate::open_raster_lazy(&path("c0.tif")).unwrap(), crate::open_raster_lazy(&path("small.tif")).unwrap());
        assert!(blend_to_path(&mut cl, &mut vl, &Default::default(), &path("sizes.tif"), &Default::default()).is_err());
        assert!(!std::path::Path::new(&path("sizes.tif")).exists());
    }
}
