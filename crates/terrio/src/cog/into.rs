// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Positioned reads into caller-owned memory. Each job owns the destination rows of one TIFF
//! block row. It reads and places every block in that row without sharing a decoder or output.

use super::{
    ChunkGeom, ChunkTables, DataType, DecodingResult, Error, LazyRaster, Level, RasterData, Result, Source, Storage, TiffRaster,
    TiffReadLimits, Window, WindowDecoder, nodata_filled, place,
};
use crate::dataset::fallible;
use crate::tiff_codecs::{ChunkCodec, ChunkScratch};
use crate::tiff_strip_rows::Rows;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

/// The sample types [`LazyRaster::read_geotiff_samples_into`] reads: the same sealed trait as
/// [`RasterElement`], under the name this read first published.
pub use crate::pixel::RasterElement as TiffSample;
use crate::pixel::RasterElement;

/// A decoded chunk's samples, if they are of type `T`.
fn decoded<T: RasterElement>(chunk: &DecodingResult) -> Option<&[T]> {
    let values: &dyn std::any::Any = match chunk {
        DecodingResult::U8(v) => v,
        DecodingResult::I8(v) => v,
        DecodingResult::U16(v) => v,
        DecodingResult::I16(v) => v,
        DecodingResult::U32(v) => v,
        DecodingResult::I32(v) => v,
        DecodingResult::U64(v) => v,
        DecodingResult::I64(v) => v,
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v,
        DecodingResult::F16(_) => return None,
    };
    values.downcast_ref::<Vec<T>>().map(Vec::as_slice)
}

/// The first value of `data`, if it holds type `T`.
fn scalar<T: RasterElement>(data: &RasterData) -> Option<T> {
    T::slice(data)?.first().copied()
}

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
    /// Prepare the positioned read with the same tables and codec as the owned reader.
    fn prepare_direct(&self, dec: &mut WindowDecoder, limits: TiffReadLimits, dtype: DataType) -> tiff::TiffResult<Option<DirectImage>> {
        let storage = self.storage(dec)?;
        let tables = self.tables(dec)?;
        let codec = match self.codec(dec, limits.chunk_bytes, tables)? {
            Some(codec) if codec.is_positioned() && (self.bits != 16 || dtype != DataType::F32) => Some(DirectCodec::Chunk(codec.clone())),
            None if storage.compression == 1 => Rows::of(dec, dtype, 1)?.map(DirectCodec::Stored),
            _ => None,
        };
        Ok(codec.map(|codec| DirectImage { storage, tables: tables.clone(), codec }))
    }
}

impl LazyRaster {
    /// Reads one GeoTIFF band into a row-major slice of its native type.
    ///
    /// The band is numbered from 1 and the overview, when given, from 0. Samples have the same
    /// values as in [`Self::read_geotiff_samples`]. Scale, offset and masks are left to the caller,
    /// and sparse blocks have the band's nodata value (or zero). Local files and immutable TIFF
    /// bytes using positioned codecs can be read concurrently through the same handle. The
    /// first read of a level prepares its block tables and codec. Each worker then places blocks
    /// into its own rows of the slice, independently of the owned reader's decoded-block cache.
    /// A caller already running on a Rayon worker decodes on that worker. Ordinary calls
    /// can use Terrio's pool (sized by `RAYON_NUM_THREADS`); overlapping outside calls stay
    /// on their own threads while the pool is in use.
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
    pub fn read_geotiff_samples_into<T: RasterElement>(
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
    fn direct_image<'a>(&self, image: &'a Level) -> Result<&'a DirectImage> {
        if image.direct.get().is_none() {
            let mut decoder = self.decoder.lock().map_err(|_| Error::InvalidData("TIFF decoder lock poisoned".into()))?;
            if image.direct.get().is_none() {
                let error = self.limits.error();
                decoder.select(image.ifd).map_err(error)?;
                let direct = image.prepare_direct(&mut decoder.decoder, self.limits, self.data_type).map_err(error)?;
                image.direct.get_or_init(|| direct);
            }
        }
        image.direct.get().and_then(Option::as_ref).ok_or_else(unsupported)
    }

    fn read_into<T: RasterElement>(&self, w: Window, band: usize, level: usize, dst: &mut [T]) -> Result<()> {
        let image = &self.levels[level];
        if let Some(i) = image.external
            && let Some(ovr) = &self.ovr
        {
            return ovr.read_into(w, band, i, dst);
        }
        let Some(source) = &self.direct_source else { return Err(unsupported()) };
        if self.cmyk || self.complex {
            return Err(unsupported());
        }
        let direct = self.direct_image(image)?;
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
        let nodata = scalar::<T>(&nodata_filled(&self.tags, self.data_type, &[band], 1, 1, 1)?)
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
        let read = |(cursor, scratch): &mut (Source, ChunkScratch), job: usize, dst: &mut [T]| -> Result<()> {
            let ty = first_y + job;
            let y0 = ty * ch;
            let dh = ch.min(image.height - y0);
            let w = Window::new(w.x, w.y.max(y0), w.width, dst.len() / w.width);
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
                        codec.read_positioned_into(cursor, idx, (dw, dh), scratch, true).map_err(err)
                    }
                    DirectCodec::Stored(rows) => {
                        let len = fallible::len(&[dw, dh, spp], "a TIFF chunk")?;
                        fallible::check_raster_buffer("a TIFF chunk", dw, dh, spp, self.data_type)?;
                        let samples = scratch.stored_samples(self.data_type, len)?;
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
                            samples,
                        )?;
                        Ok(samples)
                    }
                })?;
                let values = decoded::<T>(samples).ok_or_else(|| Error::InvalidData("TIFF chunks differ in sample type".into()))?;
                place(dst, values, &ChunkGeom { x0, y0, dw, dh, spp }, w, &[(sample, 0)]);
            }
            Ok(())
        };
        crate::par::try_for_each_owned_init(jobs, || (source.cursor(), ChunkScratch::default()), read)
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
        let mut owned = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let windows = [Window::new(13, 17, 520, 490), Window::new(239, 251, 302, 278), Window::new(121, 13, 217, 482)];
        let expected = windows.map(|window| owned.read_geotiff_samples(window, &crate::WindowOptions::default()).unwrap());
        drop(owned);
        let raster = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let renamed = dir.path().join("renamed.tif");
        std::fs::rename(path, renamed).unwrap();
        let super::super::Inner::Tiff(tiff) = &raster.inner else { panic!("expected a TIFF") };
        use std::sync::atomic::Ordering::Relaxed;
        assert!(tiff.levels[0].tables.get().is_none(), "opening does not load the window reader's tables");
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

    /// Local timing harness. Set TERRIO_READ_INTO_BENCH_TIFF to a tiled Float32 TIFF
    /// containing pixel values (linear index % 1021), and RAYON_NUM_THREADS to 8.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires a local tiled Float32 TIFF and measures elapsed time"]
    #[allow(clippy::disallowed_methods)]
    fn concurrent_destination_reads_benchmark() {
        use rayon::prelude::*;

        let path = std::env::var("TERRIO_READ_INTO_BENCH_TIFF").unwrap();
        let raster = LazyRaster::open(&path).unwrap();
        let (width, height) = (raster.width(), raster.height());
        let w = Window::full(width, height);
        for callers in [1, 8] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(callers).build().unwrap();
            let mut buffers = vec![vec![f32::NAN; width * height]; callers];
            for pass in 0..4 {
                let start = std::time::Instant::now();
                pool.install(|| {
                    buffers.par_iter_mut().for_each(|buffer| {
                        for _ in 0..4 {
                            raster.read_geotiff_samples_into(w, 1, None, buffer).unwrap();
                        }
                    });
                });
                println!("{callers} callers, pass {pass}: {:.2} ms", start.elapsed().as_secs_f64() * 1000.0);
            }
            for buffer in buffers {
                assert!(buffer.iter().enumerate().all(|(i, &v)| v == (i % 1021) as f32));
            }
        }
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
