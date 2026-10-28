//! Cloud Optimized GeoTIFF validation: the checks of rio-cogeo's `cog_validate`, itself the
//! rasterio port of GDAL's `validate_cloud_optimized_geotiff.py`, read from the TIFF structure
//! (headers and chunk offsets only, so a remote file costs a few range requests).
//!
//! A file is a valid COG when it is a TIFF whose full-resolution image, larger than 512 pixels,
//! is tiled, with tiled overviews of increasing decimation; whose image directories (IFDs) come
//! first, the main one within the first 300 bytes and each overview's after the image before it;
//! and whose image data comes after the IFDs, from the smallest overview to the full resolution.
//! A large image without overviews is a warning, as a GDAL layout invalidated by later edits
//! (`KNOWN_INCOMPATIBLE_EDITION=YES`) is an error. The errors and warnings are rio-cogeo's, word
//! for word.
//!
//! When a local file has a `.ovr` sidecar and no internal overviews, the sidecar's images are its
//! overviews, as GDAL (and so rio-cogeo) reads them, and their offsets are checked as rio-cogeo
//! checks them.
//!
//! Beyond rio-cogeo, the IFDs of masks (GDAL's internal per-dataset masks) are checked to come
//! before the image data, as GDAL lays them out in a COG. These findings go to
//! [`CogValidation::layout_notes`], not to the warnings, so that the result (strict or not) stays
//! rio-cogeo's.
//!
//! ```no_run
//! let v = terrio::validate_cog("dem.tif", false)?;
//! if !v.is_valid {
//!     eprintln!("not a COG: {}", v.errors.join("; "));
//! }
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::vfs::Location;
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// The most image directories read.
const MAX_IFDS: usize = 1024;
/// The largest chunk offset or byte count array read, in values.
const MAX_CHUNKS: u64 = 1 << 24;

/// The outcome of [`validate_cog`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CogValidation {
    /// Whether the file is a valid COG: no errors (and, with `strict`, no warnings).
    pub is_valid: bool,
    /// What makes the file not a COG.
    pub errors: Vec<String>,
    /// What a COG should do better.
    pub warnings: Vec<String>,
    /// The offsets of the IFDs of the full-resolution image, then of each overview.
    pub ifd_offsets: Vec<u64>,
    /// The offsets of the first block of the full-resolution image, then of each overview, as
    /// rio-cogeo finds them (images without a stored block are left out).
    pub data_offsets: Vec<u64>,
    /// The overview decimation factors, as rasterio reports them (`round(width / overview width)`).
    pub overview_factors: Vec<usize>,
    /// Layout findings beyond rio-cogeo's checks (masks' IFDs after image data), which do not
    /// affect `is_valid`.
    pub layout_notes: Vec<String>,
    /// GDAL's structural metadata, the `GDAL_STRUCTURAL_METADATA_SIZE` block after the header
    /// (`LAYOUT=IFDS_BEFORE_DATA`...), when the file has one.
    pub structural_metadata: Option<String>,
}

/// One image directory: what validation needs of it.
#[derive(Debug, Clone, Default)]
struct Ifd {
    offset: u64,
    width: u64,
    height: u64,
    subfile_type: u64,
    samples: u64,
    planar: u64,
    /// Tile width and height, when tiled.
    tile: Option<(u64, u64)>,
    rows_per_strip: u64,
    offsets: Vec<u64>,
    byte_counts: Vec<u64>,
}

impl Ifd {
    fn is_reduced(&self) -> bool {
        self.subfile_type & 1 != 0
    }

    fn is_mask(&self) -> bool {
        self.subfile_type & 4 != 0
    }

    /// GDAL's block size: the tile, or (width, rows per strip) for strips.
    fn block(&self) -> (u64, u64) {
        self.tile.unwrap_or((self.width, self.rows_per_strip.clamp(1, self.height.max(1))))
    }

    /// The blocks per row and column.
    fn grid(&self) -> (u64, u64) {
        let (bw, bh) = self.block();
        (self.width.div_ceil(bw.max(1)), self.height.div_ceil(bh.max(1)))
    }

    /// GDAL's `BLOCK_OFFSET_x_y` of the first band: the block's offset when it is stored.
    fn block_offset(&self, x: u64, y: u64) -> Option<u64> {
        let (across, down) = self.grid();
        if x >= across || y >= down {
            return None;
        }
        let i = (y * across + x) as usize;
        let count = self.byte_counts.get(i).copied().unwrap_or(0);
        self.offsets.get(i).copied().filter(|o| *o != 0 && count != 0)
    }

    /// The offset of the first stored block, in row order: GDAL's validation script's
    /// `get_block_offset`.
    fn first_block_offset(&self) -> Option<u64> {
        let (across, down) = self.grid();
        (0..down).flat_map(|y| (0..across).map(move |x| (x, y))).find_map(|(x, y)| self.block_offset(x, y))
    }
}

/// The parsed structure of a TIFF file.
struct Tiff {
    ifds: Vec<Ifd>,
    ghost: Option<String>,
}

/// Decodes numbers in the file's byte order.
#[derive(Clone, Copy)]
struct Endian {
    le: bool,
}

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }

    fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }

    fn u64(self, b: &[u8]) -> u64 {
        let a: [u8; 8] = b[..8].try_into().unwrap_or_default();
        if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    }

    /// The integer values of a SHORT, LONG or LONG8 tag.
    fn ints(self, typ: u16, data: &[u8]) -> Vec<u64> {
        match typ {
            1 => data.iter().map(|v| *v as u64).collect(),
            3 => data.as_chunks::<2>().0.iter().map(|c| self.u16(c) as u64).collect(),
            4 | 13 => data.as_chunks::<4>().0.iter().map(|c| self.u32(c) as u64).collect(),
            16 | 18 => data.as_chunks::<8>().0.iter().map(|c| self.u64(c)).collect(),
            _ => Vec::new(),
        }
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidData(msg.into())
}

/// Reads the IFDs of a TIFF. `None` when the stream is not a TIFF.
fn read_tiff<R: Read + Seek>(f: &mut R) -> Result<Option<Tiff>> {
    let mut header = [0u8; 16];
    f.seek(SeekFrom::Start(0))?;
    let mut n = 0;
    while n < 8 {
        let k = f.read(&mut header[n..8])?;
        if k == 0 {
            return Ok(None);
        }
        n += k;
    }
    let e = match &header[..2] {
        b"II" => Endian { le: true },
        b"MM" => Endian { le: false },
        _ => return Ok(None),
    };
    let big = match e.u16(&header[2..4]) {
        42 => false,
        43 => true,
        _ => return Ok(None),
    };
    let mut next = if big {
        f.read_exact(&mut header[8..16])?;
        e.u64(&header[8..16])
    } else {
        e.u32(&header[4..8]) as u64
    };
    let ghost = read_ghost(f, if big { 16 } else { 8 })?;
    let (entry_len, count_len, inline) = if big { (20usize, 8usize, 8usize) } else { (12, 2, 4) };
    let mut ifds: Vec<Ifd> = Vec::new();
    while next != 0 && ifds.len() < MAX_IFDS {
        if ifds.iter().any(|i| i.offset == next) {
            return Err(invalid("the TIFF's image directories loop"));
        }
        f.seek(SeekFrom::Start(next))?;
        let mut b = [0u8; 8];
        f.read_exact(&mut b[..count_len])?;
        let count = if big { e.u64(&b) } else { e.u16(&b) as u64 };
        if count > 4096 {
            return Err(invalid(format!("a TIFF image directory of {count} entries")));
        }
        let mut entries = vec![0u8; count as usize * entry_len];
        f.read_exact(&mut entries)?;
        let mut ifd = Ifd { offset: next, samples: 1, planar: 1, rows_per_strip: u64::MAX, ..Default::default() };
        let mut tile = (None, None);
        let mut values: Vec<(u16, u16, u64, Vec<u8>)> = Vec::new();
        for entry in entries.chunks_exact(entry_len) {
            let tag = e.u16(&entry[0..2]);
            if !matches!(tag, 254 | 256 | 257 | 273 | 277 | 278 | 279 | 284 | 322 | 323 | 324 | 325) {
                continue;
            }
            let typ = e.u16(&entry[2..4]);
            let (n, value) = if big { (e.u64(&entry[4..12]), &entry[12..20]) } else { (e.u32(&entry[4..8]) as u64, &entry[8..12]) };
            let size = match typ {
                1 => 1,
                3 => 2,
                4 | 13 => 4,
                16 | 18 => 8,
                _ => continue,
            };
            if n > MAX_CHUNKS {
                return Err(invalid(format!("TIFF tag {tag} has {n} values")));
            }
            let len = (n * size) as usize;
            let data = if len <= inline {
                value[..len].to_vec()
            } else {
                let off = if big { e.u64(value) } else { e.u32(value) as u64 };
                values.push((tag, typ, off, Vec::new()));
                let last = values.len() - 1;
                values[last].3 = vec![0; len];
                continue;
            };
            values.push((tag, typ, u64::MAX, data));
        }
        f.read_exact(&mut b[..if big { 8 } else { 4 }])?;
        next = if big { e.u64(&b) } else { e.u32(&b) as u64 };
        for (tag, typ, off, mut data) in values {
            if off != u64::MAX {
                f.seek(SeekFrom::Start(off))?;
                f.read_exact(&mut data)?;
            }
            let v = e.ints(typ, &data);
            let first = v.first().copied().unwrap_or(0);
            match tag {
                254 => ifd.subfile_type = first,
                256 => ifd.width = first,
                257 => ifd.height = first,
                277 => ifd.samples = first.max(1),
                278 => ifd.rows_per_strip = first,
                284 => ifd.planar = first,
                322 => tile.0 = Some(first),
                323 => tile.1 = Some(first),
                273 | 324 => ifd.offsets = v,
                279 | 325 => ifd.byte_counts = v,
                _ => {}
            }
        }
        if let (Some(w), Some(h)) = tile {
            ifd.tile = Some((w, h));
        }
        ifds.push(ifd);
    }
    Ok(Some(Tiff { ifds, ghost }))
}

/// GDAL's structural metadata block (`GDAL_STRUCTURAL_METADATA_SIZE=...`) at `offset`, if any.
fn read_ghost<R: Read + Seek>(f: &mut R, offset: u64) -> Result<Option<String>> {
    const KEY: &[u8] = b"GDAL_STRUCTURAL_METADATA_SIZE=";
    f.seek(SeekFrom::Start(offset))?;
    let mut head = [0u8; 43];
    let mut n = 0;
    while n < head.len() {
        let k = f.read(&mut head[n..])?;
        if k == 0 {
            return Ok(None);
        }
        n += k;
    }
    if !head.starts_with(KEY) {
        return Ok(None);
    }
    let size: usize = match std::str::from_utf8(&head[KEY.len()..KEY.len() + 6]).ok().and_then(|s| s.trim().parse().ok()) {
        Some(n) if n < 1 << 16 => n,
        _ => return Ok(None),
    };
    let mut body = vec![0u8; size];
    f.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

/// Validates a Cloud Optimized GeoTIFF, local or remote (HTTP(S) or S3, read with range
/// requests), as rio-cogeo's `cog_validate` does (see the `cog_validate` module documentation).
/// `strict` makes warnings errors, as rio-cogeo's.
///
/// A file that is not a TIFF is not valid (`"The file is not a GeoTIFF"`); a file that cannot be
/// read is an error.
pub fn validate_cog(path: &str, strict: bool) -> Result<CogValidation> {
    let loc = Location::parse(path);
    let (tiff, ovr): (_, Option<Tiff>) = match &loc {
        #[cfg(remote)]
        Location::Url(_) | Location::S3 { .. } => {
            let remote = loc.remote().ok_or_else(|| Error::InvalidArgument(format!("'{path}' is not remote")))?;
            let mut r = crate::http::RangeReader::open(&remote)?;
            (read_tiff(&mut r)?, None)
        }
        Location::Local(p) => {
            let mut f = std::io::BufReader::new(std::fs::File::open(p)?);
            let mut ovr = p.clone().into_os_string();
            ovr.push(".ovr");
            let ovr = std::path::Path::new(&ovr);
            // An unreadable sidecar still counts as one; GDAL would fail to open its overviews.
            let sidecar = if ovr.is_file() {
                Some(
                    read_tiff(&mut std::io::BufReader::new(std::fs::File::open(ovr)?))
                        .ok()
                        .flatten()
                        .unwrap_or(Tiff { ifds: Vec::new(), ghost: None }),
                )
            } else {
                None
            };
            (read_tiff(&mut f)?, sidecar)
        }
        _ => {
            let local = crate::vfs::resolve(path)?;
            (read_tiff(&mut std::io::BufReader::new(std::fs::File::open(local.path())?))?, None)
        }
    };
    Ok(validate(tiff, ovr, strict))
}

/// Validates parsed TIFF structure, with the structure of its `.ovr` sidecar when it has one.
fn validate(tiff: Option<Tiff>, sidecar: Option<Tiff>, strict: bool) -> CogValidation {
    let mut v = CogValidation::default();
    let Some(Tiff { ifds, ghost }) = tiff.filter(|t| !t.ifds.is_empty()) else {
        v.errors.push("The file is not a GeoTIFF".into());
        return v;
    };
    v.structural_metadata = ghost.clone();
    let main = &ifds[0];
    // GDAL's internal overviews: reduced-resolution images that are not masks, with the main
    // image's samples; and the masks of the main image and of each overview.
    let mut overviews: Vec<&Ifd> = ifds[1..].iter().filter(|i| i.is_reduced() && !i.is_mask() && i.samples == main.samples).collect();
    let masks: Vec<&Ifd> = ifds[1..].iter().filter(|i| i.is_mask()).collect();
    let external_ovr = sidecar.is_some();
    // Without internal overviews, GDAL takes the sidecar's images as the overviews.
    if overviews.is_empty()
        && let Some(sc) = &sidecar
    {
        overviews = sc.ifds.iter().filter(|i| !i.is_mask() && i.samples == main.samples).collect();
    }

    if external_ovr {
        v.errors.push("Overviews found in external .ovr file. They should be internal".into());
    }
    let factors: Vec<usize> = overviews.iter().map(|o| (main.width as f64 / o.width.max(1) as f64).round_ties_even() as usize).collect();
    v.overview_factors = factors.clone();
    let (block_w, block_h) = main.block();
    if main.width > 512 || main.height > 512 {
        if block_w == main.width {
            v.errors.push("The file is greater than 512xH or 512xW, but is not tiled".into());
        }
        if overviews.is_empty() {
            v.warnings.push("The file is greater than 512xH or 512xW, it is recommended to include internal overviews".into());
        }
    }
    if main.offset > 300 {
        v.errors.push(format!("The offset of the main IFD should be < 300. It is {} instead", main.offset));
    }
    v.ifd_offsets.push(main.offset);
    if ghost.as_deref().is_some_and(|g| g.contains("KNOWN_INCOMPATIBLE_EDITION=YES")) {
        v.errors.push(
            "This file used to have optimizations in its layout, but those have been, at least partly, invalidated by later changes".into(),
        );
    }
    let mut sorted = factors.clone();
    sorted.sort_unstable();
    if !factors.is_empty() && factors != sorted {
        v.errors.push("Overviews should be sorted".into());
    }
    for (ix, (o, dec)) in overviews.iter().zip(&factors).enumerate() {
        if *dec <= 1 {
            v.errors.push(format!("Invalid Decimation {dec} for overview level {ix}"));
        }
        let previous = *v.ifd_offsets.last().unwrap_or(&0);
        v.ifd_offsets.push(o.offset);
        if o.offset < previous {
            if ix == 0 {
                v.errors.push(format!(
                    "The offset of the IFD for overview of index {ix} is {}, whereas it should be greater than the one of the main image, which is at byte {previous}",
                    o.offset
                ));
            } else {
                v.errors.push(format!(
                    "The offset of the IFD for overview of index {ix} is {}, whereas it should be greater than the one of index {}, which is at byte {previous}",
                    o.offset,
                    ix - 1
                ));
            }
        }
    }

    // The first block of each image, scanned as rio-cogeo scans it: rows of the main image's
    // block height and columns of its block width swapped (rasterio's block shapes are (rows,
    // cols)), overviews by the main image's block size over `size / factor` pixels.
    let scan = |ifd: &Ifd, width: u64, height: u64| -> Option<u64> {
        let yblocks = height.div_ceil(block_w.max(1));
        let xblocks = width.div_ceil(block_h.max(1));
        (0..yblocks).flat_map(|y| (0..xblocks).map(move |x| (x, y))).find_map(|(x, y)| ifd.block_offset(x, y))
    };
    if let Some(off) = scan(main, main.width, main.height) {
        v.data_offsets.push(off);
    }
    for (o, dec) in overviews.iter().zip(&factors) {
        let d = (*dec as u64).max(1);
        if let Some(off) = scan(o, main.width / d, main.height / d) {
            v.data_offsets.push(off);
        }
    }
    let data = &v.data_offsets;
    let mut errors = Vec::new();
    if let (Some(last_data), Some(last_ifd)) = (data.last(), v.ifd_offsets.last())
        && last_data < last_ifd
    {
        errors.push(if overviews.is_empty() {
            "The offset of the first block of the image should be after its IFD".to_string()
        } else {
            "The offset of the first block of the smallest overview should be after its IFD".to_string()
        });
    }
    for i in (1..data.len().saturating_sub(1)).rev() {
        if data[i] < data[i + 1] {
            errors.push(format!(
                "The offset of the first block of overview of index {} should be after the one of the overview of index {i}",
                i - 1
            ));
        }
    }
    if data.len() >= 2 && data[0] < data[1] {
        errors.push(format!(
            "The offset of the first block of the main resolution image should be after the one of the overview of index {}",
            overviews.len() as i64 - 1
        ));
    }
    v.errors.extend(errors);
    for (ix, o) in overviews.iter().enumerate() {
        if (o.width > 512 || o.height > 512) && o.block().0 == o.width {
            v.errors.push(format!("Overview of index {ix} is not tiled"));
        }
    }

    // Masks, as GDAL's COG driver lays them out (beyond rio-cogeo): IFDs before any image data,
    // and each mask's data before the data of the image it masks.
    let first_data = ifds.iter().filter_map(|i| i.first_block_offset()).min();
    for (k, m) in masks.iter().enumerate() {
        let which = if m.is_reduced() { format!("mask of overview {k}") } else { "mask of the main image".into() };
        if first_data.is_some_and(|d| m.offset > d) {
            v.layout_notes.push(format!("The IFD of the {which} is at byte {}, after image data", m.offset));
        }
    }
    v.is_valid = v.errors.is_empty() && !(strict && !v.warnings.is_empty());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(raster: &wbraster::Raster, options: &crate::RasterWriteOptions) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.tif").to_string_lossy().into_owned();
        crate::write_raster(raster, &p, options).unwrap();
        (dir, p)
    }

    fn raster(cols: usize, rows: usize) -> wbraster::Raster {
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands: 1,
            cell_size: 1.0,
            nodata: -9999.0,
            data_type: wbraster::DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(32610),
            ..Default::default()
        };
        let data = wbraster::raster::RasterData::F32((0..cols * rows).map(|i| (i % 1000) as f32).collect());
        wbraster::Raster::from_data_native(cfg, data).unwrap()
    }

    fn driver(d: crate::formats::RasterDriver, co: &[(&str, &str)]) -> crate::RasterWriteOptions {
        crate::RasterWriteOptions {
            driver: Some(d),
            creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn terrio_cogs_are_valid() {
        use crate::formats::RasterDriver::Cog;
        for (cols, rows) in [(100, 80), (1300, 700), (2100, 1500)] {
            let (_d, p) = write(&raster(cols, rows), &driver(Cog, &[("COMPRESS", "DEFLATE")]));
            let v = validate_cog(&p, true).unwrap();
            assert!(v.is_valid, "{cols}x{rows}: {v:?}");
            assert!(v.structural_metadata.as_deref().unwrap().contains("LAYOUT=IFDS_BEFORE_DATA"));
            let expected: Vec<usize> = (1..v.ifd_offsets.len()).map(|i| 1 << i).collect();
            assert_eq!(v.overview_factors, expected);
        }
    }

    #[test]
    fn geotiff_layouts_are_not_cogs() {
        use crate::formats::RasterDriver::GTiff;
        // Strips, larger than 512 pixels: not tiled, no overviews, IFD after the data.
        let (_d, p) = write(&raster(1300, 700), &driver(GTiff, &[]));
        let v = validate_cog(&p, false).unwrap();
        assert!(!v.is_valid);
        assert!(v.errors.contains(&"The file is greater than 512xH or 512xW, but is not tiled".to_string()), "{v:?}");
        assert!(v.warnings.iter().any(|w| w.contains("recommended to include internal overviews")));
        // Small, tiled, and laid out as a GeoTIFF: overviews after the full resolution.
        let mut o = driver(GTiff, &[("TILED", "YES")]);
        o.overviews = vec![2, 4];
        let (_d, p) = write(&raster(1300, 700), &o);
        let v = validate_cog(&p, false).unwrap();
        assert!(!v.is_valid, "{v:?}");
    }

    #[test]
    fn small_files_and_strictness() {
        use crate::formats::RasterDriver::GTiff;
        // Small strips are fine; a large tiled file without overviews only warns.
        let (_d, p) = write(&raster(100, 80), &driver(GTiff, &[]));
        assert!(validate_cog(&p, false).unwrap().errors.iter().all(|e| !e.contains("tiled")));
        let o = driver(crate::formats::RasterDriver::Cog, &[]);
        let (_d, p) = write(&raster(1300, 700), &crate::RasterWriteOptions { tile_size: Some(2048), ..o });
        let v = validate_cog(&p, false).unwrap();
        assert!(v.is_valid, "{v:?}");
        assert_eq!(v.warnings.len(), 1);
        assert!(!validate_cog(&p, true).unwrap().is_valid);
    }

    #[test]
    fn external_overviews_are_reported_and_used() {
        use crate::formats::RasterDriver::{Cog, GTiff};
        let (dir, p) = write(&raster(1300, 700), &driver(Cog, &[]));
        // A COG beside a sidecar holding another COG's images: internal overviews still win.
        std::fs::copy(&p, format!("{p}.ovr")).unwrap();
        let v = validate_cog(&p, false).unwrap();
        assert_eq!(v.errors, vec!["Overviews found in external .ovr file. They should be internal".to_string()]);
        // A tiled GeoTIFF without internal overviews takes the sidecar's.
        let tiled = dir.path().join("tiled.tif").to_string_lossy().into_owned();
        crate::write_raster(&raster(1300, 700), &tiled, &crate::RasterWriteOptions { tile_size: Some(256), ..driver(GTiff, &[]) }).unwrap();
        std::fs::copy(&p, format!("{tiled}.ovr")).unwrap();
        let v = validate_cog(&tiled, false).unwrap();
        assert_eq!(v.overview_factors.len(), 3, "{v:?}");
        assert!(v.warnings.is_empty());
    }

    #[test]
    fn terrio_masks_need_no_layout_notes() {
        use crate::formats::RasterDriver::GTiff;
        let mut o = driver(GTiff, &[("TILED", "YES")]);
        o.mask = Some(vec![1; 600 * 520]);
        let (_d, p) = write(&raster(600, 520), &o);
        let v = validate_cog(&p, true).unwrap();
        // terrio's GeoTIFF writer puts IFDs first: nothing to note.
        assert!(v.layout_notes.is_empty(), "{v:?}");
    }

    #[test]
    fn not_a_tiff() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.tif");
        std::fs::write(&p, b"not a tiff at all").unwrap();
        let v = validate_cog(p.to_str().unwrap(), false).unwrap();
        assert!(!v.is_valid);
        assert_eq!(v.errors, vec!["The file is not a GeoTIFF".to_string()]);
        assert!(validate_cog(dir.path().join("missing.tif").to_str().unwrap(), false).is_err());
    }
}
