// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Positioned reads into caller-owned memory. Each job owns the destination rows of one TIFF
//! block row. It reads and places every block in that row without sharing a decoder or output.

use super::{
    ChunkGeom, ChunkTables, DataType, Decoder, DecodingResult, Error, LazyRaster, Level, PatchedReader, RasterData, Result, Source,
    Storage, Tag, TiffRaster, TiffReadLimits, Window, nodata_filled, place,
};
use crate::dataset::fallible;
use crate::tiff_codecs::ChunkCodec;
use crate::tiff_strip_rows::Rows;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

mod sealed {
    use super::{DecodingResult, RasterData};

    pub trait Sealed: Sized {
        fn decoded(chunk: &DecodingResult) -> Option<&[Self]>;
        fn scalar(data: RasterData) -> Option<Self>;
    }
}

/// A numeric sample type for [`LazyRaster::read_geotiff_samples_into`].
///
/// Implemented for the ten primitive types held by [`RasterData`].
pub trait TiffSample: sealed::Sealed + Copy + Send + Sync {
    /// The GeoTIFF sample type accepted for this Rust type.
    const DATA_TYPE: DataType;
}

macro_rules! samples {
    ($($ty:ty => $variant:ident),* $(,)?) => {$(
        impl sealed::Sealed for $ty {
            fn decoded(chunk: &DecodingResult) -> Option<&[Self]> {
                match chunk {
                    DecodingResult::$variant(values) => Some(values),
                    _ => None,
                }
            }

            fn scalar(data: RasterData) -> Option<Self> {
                match data {
                    RasterData::$variant(values) => values.first().copied(),
                    _ => None,
                }
            }
        }

        impl TiffSample for $ty {
            const DATA_TYPE: DataType = DataType::$variant;
        }
    )*};
}
samples!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);

pub(super) enum DirectSource {
    File(crate::geotiff::FileCursor),
    Memory(super::SharedBytes),
}

impl DirectSource {
    pub(super) fn new(source: &Source) -> Option<Self> {
        match source {
            Source::File(file) => Some(Self::File(file.get_ref().clone())),
            Source::Memory(bytes) => Some(Self::Memory(bytes.get_ref().clone())),
            _ => None,
        }
    }

    fn cursor(&self) -> Source {
        match self {
            Self::File(file) => Source::File(crate::vfs::NearSeek::new(file.clone())),
            Self::Memory(bytes) => Source::Memory(std::io::Cursor::new(bytes.clone())),
        }
    }
}

enum DirectCodec {
    Stored(Rows),
    Chunk(Arc<ChunkCodec>),
}

pub(super) struct DirectImage {
    storage: Storage,
    tables: Arc<ChunkTables>,
    codec: DirectCodec,
}

impl Level {
    /// Retain the block tables and codec selected from the decoder's current image. Share these
    /// with the owned path, which still handles codecs that require a TIFF decoder.
    pub(super) fn prepare_direct(&mut self, dec: &mut Decoder<PatchedReader<Source>>, limits: TiffReadLimits, dtype: DataType) {
        let mut prepare = || -> tiff::TiffResult<_> {
            let storage = Storage {
                planar: dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)? == Some(2),
                compression: dec.find_tag_unsigned::<u16>(Tag::Compression)?.unwrap_or(1),
            };
            let tables = Arc::new(ChunkTables::read(dec)?);
            let codec = ChunkCodec::new(dec, limits.chunk_bytes, Some(tables.clone()))?.map(Arc::new);
            let direct = match &codec {
                Some(codec) if codec.is_positioned() && (self.bits != 16 || dtype != DataType::F32) => {
                    Some(DirectCodec::Chunk(codec.clone()))
                }
                None if storage.compression == 1 => Rows::of(dec, dtype, 1)?.map(DirectCodec::Stored),
                _ => None,
            };
            Ok((storage, tables, codec, direct))
        };
        if let Ok((storage, tables, codec, direct)) = prepare() {
            self.storage = Some(storage);
            self.tables = Some(tables.clone());
            self.codec = Some(codec);
            self.direct = direct.map(|codec| DirectImage { storage, tables, codec });
        }
    }
}

impl LazyRaster {
    /// Reads one GeoTIFF band into a row-major slice of its native type.
    ///
    /// The band is numbered from 1 and the overview, when given, from 0. Samples have the same
    /// values as in [`Self::read_geotiff_samples`]. Scale, offset and masks are left to the caller,
    /// and sparse blocks have the band's nodata value (or zero). Local files and immutable TIFF
    /// bytes using positioned codecs can be read concurrently through the same handle. Each
    /// worker places decoded blocks into its own rows of the slice, independently of the
    /// decoded-block cache used by the owned reader.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] for a missing band or overview, an out-of-bounds window,
    /// a mismatched sample type, or a slice whose length is other than `width * height`.
    /// [`Error::Unsupported`] indicates a source or codec that needs the existing mutable read
    /// API (including HTTP range readers, shared aliases, and CMYK or complex rasters).
    /// On an I/O or decode error the destination may be partially written.
    ///
    /// ```no_run
    /// use terrio::{Window, open_raster_lazy};
    /// let raster = open_raster_lazy("dem.tif")?;
    /// let mut elevations = vec![0f32; 128 * 64];
    /// raster.read_geotiff_samples_into(Window::new(20, 30, 128, 64), 1, None, &mut elevations)?;
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_geotiff_samples_into<T: TiffSample>(
        &self,
        window: Window,
        band: usize,
        overview: Option<usize>,
        dst: &mut [T],
    ) -> Result<()> {
        let _scope = self.enter_asset_read()?;
        let (width, height) = self.level_size(overview)?;
        window.check(width, height)?;
        if band == 0 || band > self.band_count() {
            return Err(Error::InvalidArgument(format!("no band {band}: the raster has {} band(s)", self.band_count())));
        }
        let dtype = self.band_data_type(band);
        if T::DATA_TYPE != dtype {
            return Err(Error::InvalidArgument(format!(
                "destination type {} differs from band {band}'s {}",
                T::DATA_TYPE.as_str(),
                dtype.as_str(),
            )));
        }
        if window.width.checked_mul(window.height) != Some(dst.len()) {
            return Err(Error::InvalidArgument(format!(
                "destination has {} samples for a {} x {} window",
                dst.len(),
                window.width,
                window.height,
            )));
        }
        let super::Inner::Tiff(tiff) = &self.inner else { return Err(unsupported()) };
        tiff.read_into(window, band, overview.map_or(0, |i| i + 1), dst)
    }
}

fn unsupported() -> Error {
    Error::Unsupported("this raster or TIFF compression needs an owned window read".into())
}

impl TiffRaster {
    fn read_into<T: TiffSample>(&self, w: Window, band: usize, level: usize, dst: &mut [T]) -> Result<()> {
        let image = &self.levels[level];
        if let Some(i) = image.external
            && let Some(ovr) = &self.ovr
        {
            return ovr.read_into(w, band, i, dst);
        }
        let (Some(source), Some(direct)) = (&self.direct_source, &image.direct) else { return Err(unsupported()) };
        let (cw, ch) = image.block;
        let spp = if direct.storage.planar { 1 } else { self.samples };
        // Large strips and edge tiles can need the owned reader's strip-row or cropped tile
        // fallback. Choose the fallback before writing any destination rows.
        if cw
            .checked_mul(ch)
            .and_then(|n| n.checked_mul(spp * T::DATA_TYPE.size_bytes()))
            .is_none_or(|bytes| bytes > self.limits.chunk_bytes)
        {
            return Err(unsupported());
        }
        let nodata = T::scalar(nodata_filled(&self.tags, self.data_type, &[band], 1, 1, 1)?)
            .ok_or_else(|| Error::InvalidData("TIFF nodata type differs from band type".into()))?;
        let first_y = w.y / ch;
        let first_rows = (ch - w.y % ch).min(w.height);
        let (first, rest) = dst.split_at_mut(first_rows * w.width);
        let mut jobs = Vec::new();
        fallible::reserve(&mut jobs, w.height.div_ceil(ch) + 1, "TIFF block rows")?;
        jobs.push(first);
        if !rest.is_empty() {
            jobs.extend(rest.chunks_mut(ch.saturating_mul(w.width)));
        }
        let band0 = band - 1;
        let (plane, sample) = if direct.storage.planar { (band0, 0) } else { (0, band0) };
        let across = image.width.div_ceil(cw);
        let down = image.height.div_ceil(ch);
        let plane_start = plane
            .checked_mul(down)
            .and_then(|n| n.checked_mul(across))
            .ok_or_else(|| Error::InvalidData("TIFF chunk index overflow".into()))?;
        let err = self.limits.error();
        let read = |job: usize, dst: &mut [T]| -> Result<()> {
            let ty = first_y + job;
            let y0 = ty * ch;
            let dh = ch.min(image.height - y0);
            let w = Window::new(w.x, w.y.max(y0), w.width, dst.len() / w.width);
            let mut cursor = source.cursor();
            for tx in w.x / cw..=(w.x + w.width - 1) / cw {
                let x0 = tx * cw;
                let dw = cw.min(image.width - x0);
                let idx = ty
                    .checked_mul(across)
                    .and_then(|n| n.checked_add(tx))
                    .and_then(|n| n.checked_add(plane_start))
                    .ok_or_else(|| Error::InvalidData("TIFF chunk index overflow".into()))?;
                let stored = direct.tables.get(idx).ok_or_else(|| Error::InvalidData(format!("no TIFF chunk {idx}")))?;
                if stored.1 == 0 {
                    let x = x0.max(w.x) - w.x;
                    let end = (x0 + dw).min(w.x + w.width) - w.x;
                    for row in dst.chunks_mut(w.width) {
                        row[x..end].fill(nodata);
                    }
                    continue;
                }
                crate::geotiff::check_stored_chunks(direct.storage.compression, [stored], self.file_len)?;
                let samples = crate::geotiff::guard_chunk_decode(idx, || match &direct.codec {
                    DirectCodec::Chunk(codec) => {
                        let bytes = codec.positioned_bytes((dw, dh)).map_err(err)?;
                        fallible::check_usable(Some(bytes), || format!("decoded TIFF chunk {idx} is too large to allocate"))
                            .map_err(Error::ResourceLimit)?;
                        codec.read_positioned(&mut cursor, idx, (dw, dh)).map_err(err)
                    }
                    DirectCodec::Stored(rows) => {
                        let len = fallible::len(&[dw, dh, spp], "a TIFF chunk")?;
                        fallible::check_raster_buffer("a TIFF chunk", dw, dh, spp, self.data_type)?;
                        let mut samples = crate::tiff_strip_rows::sample_buffer(self.data_type, len)?;
                        let mut strip = rows.strip(stored, dw, spp, dh);
                        strip.read(
                            &mut |offset, bytes| {
                                if dw == cw {
                                    cursor.seek(SeekFrom::Start(offset))?;
                                    cursor.read_exact(bytes)?;
                                } else {
                                    // As the owned decoder does, read only the visible columns of
                                    // each row of an uncompressed edge tile, skipping padding.
                                    let row_bytes = dw * spp * T::DATA_TYPE.size_bytes();
                                    let stride = cw * spp * T::DATA_TYPE.size_bytes();
                                    for (i, row) in bytes.chunks_exact_mut(row_bytes).enumerate() {
                                        let at = offset
                                            .checked_add(i as u64 * stride as u64)
                                            .ok_or_else(|| Error::InvalidData("TIFF tile offset overflow".into()))?;
                                        cursor.seek(SeekFrom::Start(at))?;
                                        cursor.read_exact(row)?;
                                    }
                                }
                                Ok(())
                            },
                            0,
                            &mut samples,
                        )?;
                        Ok(samples)
                    }
                })?;
                let values = T::decoded(&samples).ok_or_else(|| Error::InvalidData("TIFF chunks differ in sample type".into()))?;
                place(dst, values, &ChunkGeom { x0, y0, dw, dh, spp }, w, &[(sample, 0)]);
            }
            Ok(())
        };
        #[cfg(threads)]
        if jobs.len() > 1 && crate::par::threads() > 1 {
            use rayon::prelude::*;
            // Always use Terrio's fork-aware pool.
            #[allow(clippy::disallowed_methods)]
            return crate::par::install(|| jobs.into_par_iter().enumerate().try_for_each(|(i, dst)| read(i, dst)));
        }
        jobs.into_iter().enumerate().try_for_each(|(i, dst)| read(i, dst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cog(path: &std::path::Path) {
        let (cols, rows) = (541, 529);
        let data = RasterData::F32((0..cols * rows).map(|v| v as f32 - 1000.0).collect());
        let cfg = crate::RasterConfig { cols, rows, bands: 1, data_type: DataType::F32, nodata: -999.0, ..Default::default() };
        let raster = crate::RasterDataset::new(crate::Raster::from_data_native(cfg, data).unwrap(), false);
        let options = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), ..Default::default() };
        crate::write_raster_dataset(&raster, path.to_str().unwrap(), &options).unwrap();
    }

    #[test]
    fn concurrent_destination_reads_keep_the_original_file_and_use_no_worker_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        write_cog(&path);
        let mut raster = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let windows = [Window::new(13, 17, 520, 490), Window::new(239, 251, 302, 278), Window::new(121, 13, 217, 482)];
        let expected = windows.map(|window| raster.read_geotiff_samples(window, &crate::WindowOptions::default()).unwrap());
        let renamed = dir.path().join("renamed.tif");
        std::fs::rename(path, renamed).unwrap();
        let super::super::Inner::Tiff(tiff) = &raster.inner else { panic!("expected a TIFF") };
        use std::sync::atomic::Ordering::Relaxed;
        let before = tiff.workers.opened.load(Relaxed);
        let barrier = std::sync::Barrier::new(windows.len());
        std::thread::scope(|scope| {
            for (window, expected) in windows.into_iter().zip(expected) {
                let (raster, barrier) = (&raster, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..3 {
                        let mut dst = vec![f32::NAN; window.width * window.height];
                        raster.read_geotiff_samples_into(window, 1, None, &mut dst).unwrap();
                        assert_eq!(dst, expected.as_f32_slice().unwrap());
                    }
                });
            }
        });
        assert_eq!(tiff.workers.opened.load(Relaxed), before, "no new worker decoders");
    }

    #[test]
    fn destination_shape_and_type_are_checked_before_pixels_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        write_cog(&path);
        let raster = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let window = Window::new(0, 0, 2, 2);
        let mut too_short = [42f32; 3];
        let mut wrong_type = [42u32; 4];
        let mut pixels = [42f32; 4];
        assert!(matches!(raster.read_geotiff_samples_into(window, 1, None, &mut too_short), Err(Error::InvalidArgument(_))));
        assert!(matches!(raster.read_geotiff_samples_into(window, 1, None, &mut wrong_type), Err(Error::InvalidArgument(_))));
        assert!(matches!(raster.read_geotiff_samples_into(window, 0, None, &mut pixels), Err(Error::InvalidArgument(_))));
        assert_eq!(too_short, [42.0; 3]);
        assert_eq!(wrong_type, [42; 4]);
        assert_eq!(pixels, [42.0; 4]);
    }

    #[test]
    fn destination_overview_uses_the_overview_block_grid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        write_cog(&path);
        let mut raster = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let (w, h) = raster.overviews()[0];
        let win = Window::new(3, 5, w - 3, h - 5);
        let mut dst = vec![0.0f32; win.width * win.height];
        raster.read_geotiff_samples_into(win, 1, Some(0), &mut dst).unwrap();
        let want = raster.read_geotiff_samples(win, &crate::WindowOptions { bands: vec![1], overview: Some(0) }).unwrap();
        assert_eq!(dst, want.as_f32_slice().unwrap());
    }
}
