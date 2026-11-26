//! Zstandard, with the pure-Rust `zrip` or, in native builds, the zstd C library. The choice is
//! made per format, by opt-in feature: GeoTIFF/COG chunks use the C library with
//! `libzstd-geotiff`, EPT tiles with `libzstd-ept` and GeoParquet pages with
//! `libzstd-geoparquet` (`libzstd` turns on all three). WebAssembly is always `zrip`, and so are
//! Zarr and terrio-crs's bundled tables. Both read every frame of a stream (skippable frames
//! skipped), and each reads what the other writes.
//!
//! CLB fork: without the C library, the levels above `zrip`'s highest (4) are written by
//! `zstd-rust` with the `zstd-rust` feature (on by default), rather than at level 4 (see
//! [`pure::compress`]). `zrip` still writes every level up to 4 and decodes everything.

use std::io::{Error, ErrorKind, Read, Result};

/// The `cfg` of a format's Zstandard: the C library with `$feature` natively, `zrip` otherwise.
macro_rules! zstd_for {
    ($(#[$m:meta])* $name:ident, $feature:literal, $($item:ident),+) => {
        $(#[$m])*
        pub(crate) mod $name {
            #[cfg(all(feature = $feature, not(target_family = "wasm")))]
            #[allow(unused_imports)]
            pub(crate) use super::libzstd::{$($item),+};
            #[cfg(not(all(feature = $feature, not(target_family = "wasm"))))]
            #[allow(unused_imports)]
            pub(crate) use super::pure::{$($item),+};
        }
    };
}

zstd_for!(
    /// GeoTIFF/COG chunks: `frames` reads ZSTD chunks, `compress` writes ZSTD and LERC_ZSTD ones.
    geotiff,
    "libzstd-geotiff",
    compress,
    frames,
    frames_into
);
zstd_for!(
    /// EPT tiles.
    ept,
    "libzstd-ept",
    compress,
    decompress
);
zstd_for!(
    /// The GeoParquet pages terrio compresses and decompresses itself.
    geoparquet,
    "libzstd-geoparquet",
    compress,
    decompress
);

// Upstream's names: GeoTIFF's `compress` and `frames`, EPT's `decompress`.
#[allow(unused_imports)]
pub(crate) use ept::decompress;
#[allow(unused_imports)]
pub(crate) use geotiff::{compress, frames, frames_into};

/// The output limit of a `zrip` streaming decoder read for its first `max` bytes at most: one
/// block more, since the block holding the last byte read is decoded whole. The decoder reserves
/// no history past its limit, so a frame header declaring a larger content size or window
/// reserves no more.
pub(crate) fn read_limit(max: usize) -> usize {
    max.saturating_add(zrip::frame::MAX_BLOCK_SIZE)
}

/// The error of a stream decompressing to more than `limit` bytes.
fn too_long(limit: usize) -> Error {
    Error::new(ErrorKind::InvalidData, format!("Zstandard: decompresses to more than {limit} bytes"))
}

#[cfg(all(any(feature = "libzstd-geotiff", feature = "libzstd-ept", feature = "libzstd-geoparquet"), not(target_family = "wasm")))]
// Each item is used by the formats whose feature is on.
#[allow(dead_code)]
mod libzstd {
    use super::*;

    /// Compresses `data` at `level`.
    pub(crate) fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
        zstd::bulk::compress(data, level)
    }

    /// Decompresses every frame of `data`, to at most `limit` bytes (more is an error, rather
    /// than an allocation of whatever a small stream expands to).
    ///
    /// A stream whose frames all record their size (as the C library's do) is decompressed in one
    /// call into a vector of that size; a frame decompressing past its recorded size is an error.
    /// Others are decompressed as a stream.
    pub(crate) fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
        match recorded_size(data) {
            Some(size) if size > limit as u64 => return Err(too_long(limit)),
            Some(size) => return zstd::bulk::decompress(data, size as usize),
            None => {}
        }
        let mut out = Vec::new();
        zstd::stream::read::Decoder::with_buffer(data)?.take((limit as u64).saturating_add(1)).read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(too_long(limit));
        }
        Ok(out)
    }

    /// The first `max` bytes `data` decompresses to, frame after frame, skippable frames
    /// skipped, as [`super::pure::frames`]: the rest is not decoded.
    ///
    /// A stream recording a size of no more than `max` is decompressed in one call, as by
    /// [`decompress`]; others as a stream, to `max` bytes.
    pub(crate) fn frames(data: &[u8], max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        frames_into(data, max, &mut out)?;
        Ok(out)
    }

    /// [`frames`] into `out`, replacing what it holds and keeping its allocation.
    pub(crate) fn frames_into(data: &[u8], max: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        if let Some(size) = recorded_size(data).filter(|&size| size <= max as u64) {
            out.try_reserve(size as usize).map_err(|e| Error::new(ErrorKind::OutOfMemory, e))?;
            zstd::bulk::Decompressor::new()?.decompress_to_buffer(data, out)?;
            return Ok(());
        }
        zstd::stream::read::Decoder::with_buffer(data)?.take(max as u64).read_to_end(out)?;
        Ok(())
    }

    /// The bytes `data` decompresses to as its frames record them (a skippable frame's are none),
    /// or `None` when a frame does not record them, or is truncated or not a frame.
    fn recorded_size(mut data: &[u8]) -> Option<u64> {
        use zstd::zstd_safe::{find_frame_compressed_size, get_frame_content_size};
        if data.is_empty() {
            return None;
        }
        let mut total = 0u64;
        while !data.is_empty() {
            let size = get_frame_content_size(data).ok()??;
            let len = find_frame_compressed_size(data).ok()?;
            if len == 0 || len > data.len() {
                return None;
            }
            total = total.checked_add(size)?;
            data = &data[len..];
        }
        Some(total)
    }
}

/// `zrip`: Zarr's, the other formats' without their `libzstd-*` feature, and every format's in
/// WebAssembly.
// Each item is used by the formats without their feature.
#[allow(dead_code)]
pub(crate) mod pure {
    use super::*;
    use zrip::DecompressError;

    /// Compresses `data` at `level`: 0 is zstd's default, 3. `zrip` writes levels -7 to 4 (a
    /// lower level writes -7). A higher level is `zstd-rust`'s with the `zstd-rust` feature (CLB
    /// fork; see [`rust_level`]), and `zrip`'s level 4 without it.
    pub(crate) fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
        let level = if level == 0 { 3 } else { level };
        #[cfg(feature = "zstd-rust")]
        if level > zrip::MAX_LEVEL {
            return Ok(rust::compress(data, rust_level(level)));
        }
        let level = level.clamp(zrip::MIN_LEVEL, zrip::MAX_LEVEL);
        zrip::compress(data, level).map_err(|e| Error::new(ErrorKind::InvalidInput, format!("Zstandard: {e}")))
    }

    /// The `zstd-rust` level for a zstd level above `zrip`'s highest (CLB fork): 5 to 9 its
    /// `Better` (about zstd's 7 to 9, and smaller than `zrip`'s 4), 10 and above its `Best`.
    #[cfg(feature = "zstd-rust")]
    pub(crate) fn rust_level(level: i32) -> zstd_rust::encoding::CompressionLevel {
        use zstd_rust::encoding::CompressionLevel as L;
        if level >= 10 { L::Best } else { L::Better }
    }

    /// `zstd-rust`'s encoder (CLB fork), for the levels above `zrip`'s.
    #[cfg(feature = "zstd-rust")]
    mod rust {
        use std::cell::RefCell;
        use zstd_rust::encoding::{CompressionLevel, Compressor};

        thread_local! {
            /// The compressor of this thread, kept from chunk to chunk: its tables (megabytes at
            /// these levels) are then allocated once per thread, not once per chunk.
            static COMPRESSOR: RefCell<Option<Compressor>> = const { RefCell::new(None) };
        }

        /// One frame holding `data`, which records its size, as the C library's and `zrip`'s do.
        pub(super) fn compress(data: &[u8], level: CompressionLevel) -> Vec<u8> {
            COMPRESSOR.with(|compressor| {
                let mut compressor = compressor.borrow_mut();
                let compressor = compressor.get_or_insert_with(|| Compressor::new(level));
                compressor.set_compression_level(level);
                compressor.compress(data)
            })
        }
    }

    /// Decompresses every frame of `data`, to at most `limit` bytes (more is an error).
    pub(crate) fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
        zrip::decompress_with_limit(data, limit).map_err(|e| match e {
            DecompressError::OutputTooSmall => too_long(limit),
            e => invalid(e),
        })
    }

    /// The first `max` bytes `data` decompresses to, frame after frame, skippable frames skipped:
    /// a stream decoding to more is decoded again as a stream, up to `max` bytes, the rest not
    /// decoded.
    pub(crate) fn frames(data: &[u8], max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        frames_into(data, max, &mut out)?;
        Ok(out)
    }

    /// [`frames`] into `out`, replacing what it holds and keeping its allocation.
    pub(crate) fn frames_into(data: &[u8], max: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        match zrip::DecompressContext::new().decompress_into_with_limit(data, out, max) {
            Err(DecompressError::OutputTooSmall) => {
                out.clear();
                zrip::FrameDecoder::with_limit(data, read_limit(max)).take(max as u64).read_to_end(out)?;
                Ok(())
            }
            r => r.map(drop).map_err(invalid),
        }
    }

    fn invalid(e: DecompressError) -> Error {
        Error::new(ErrorKind::InvalidData, format!("Zstandard: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skippable frame of `n` bytes.
    fn skippable(n: usize) -> Vec<u8> {
        let mut out = 0x184D_2A50u32.to_le_bytes().to_vec();
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend(std::iter::repeat_n(7, n));
        out
    }

    fn data(n: usize, seed: u32) -> Vec<u8> {
        (0..n as u32).map(|i| (i.wrapping_mul(seed) >> 3) as u8).collect()
    }

    #[test]
    fn frames_are_read_one_after_another() {
        let (a, b) = (data(100_000, 31), data(70_000, 97));
        let stream = [pure::compress(&a, 3).unwrap(), skippable(9), pure::compress(&b, 3).unwrap()].concat();
        let both = [a.clone(), b.clone()].concat();
        assert_eq!(pure::decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(ept::decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(geoparquet::decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(geotiff::frames(&stream, usize::MAX).unwrap(), both);
        // At most `max` bytes, the rest not decoded.
        assert_eq!(pure::frames(&stream, 120_000).unwrap(), both[..120_000]);
        // A truncated skippable frame is an error.
        let cut = [pure::compress(&a, 3).unwrap(), skippable(9)[..12].to_vec()].concat();
        assert!(pure::decompress(&cut, usize::MAX).is_err());
    }

    #[test]
    fn unlimited_output_limit_does_not_overflow() {
        let input = b"a finite frame with an unlimited caller policy";
        let compressed = pure::compress(input, 1).unwrap();
        assert_eq!(decompress(&compressed, usize::MAX).unwrap(), input);
        assert_eq!(geoparquet::decompress(&compressed, usize::MAX).unwrap(), input);
        assert_eq!(pure::decompress(&compressed, usize::MAX).unwrap(), input);
    }

    #[test]
    fn small_streams_expanding_past_the_limit_are_errors() {
        // 64 MiB of zeros in a few kilobytes.
        let bomb = pure::compress(&vec![0; 64 << 20], 3).unwrap();
        assert!(bomb.len() < 1 << 20);
        for e in [
            pure::decompress(&bomb, 1 << 20).unwrap_err(),
            decompress(&bomb, 1 << 20).unwrap_err(),
            geoparquet::decompress(&bomb, 1 << 20).unwrap_err(),
        ] {
            assert!(e.to_string().contains("more than 1048576 bytes"), "{e}");
        }
        assert_eq!(decompress(&bomb, 64 << 20).unwrap().len(), 64 << 20);
        assert_eq!(geoparquet::decompress(&bomb, 64 << 20).unwrap().len(), 64 << 20);
    }

    /// The bound of ZSTD GeoTIFF chunks (`frames`, given the chunk's size): a stream expanding far
    /// past it is decoded to that many bytes and no further, by the implementation in use and by
    /// `zrip`. The values decoded are the same either way: only the length shows the bound.
    #[test]
    fn frames_decode_no_further_than_their_bound() {
        // 64 MiB of zeros, then a second frame, in a few kilobytes.
        let bomb = [pure::compress(&vec![0; 64 << 20], 3).unwrap(), pure::compress(&[1; 100], 3).unwrap()].concat();
        assert!(bomb.len() < 1 << 20);
        for max in [0, 1, 4096, 1 << 20] {
            for (decoder, out) in [("in use", frames(&bomb, max).unwrap()), ("zrip", pure::frames(&bomb, max).unwrap())] {
                assert_eq!(out.len(), max, "{decoder}");
                assert!(out.iter().all(|&b| b == 0), "{decoder}");
            }
        }
        assert_eq!(frames(&bomb, usize::MAX).unwrap().len(), (64 << 20) + 100);
    }

    /// 300 kB of words drawn at random from a list of common English ones: repetitions far apart,
    /// which `zrip`'s level 4, `zstd-rust`'s `Better` and its `Best` each find more of.
    fn words() -> Vec<u8> {
        let list: Vec<&str> = "the of and to a in is that for it as was with be by on not he this are or his from at \
            which but have an they you were her she there would their we him been has when who will more no if out so said \
            what up its about into than them can only other new some could time these two may then do first any my now such \
            like our over man me even most made after also did many before must through back years where much your way well \
            down should because each just those people how too little state good very make world still own see men work long \
            here get both between life being under never day same another know while last might us great old year off come \
            since against go came right used take three"
            .split_whitespace()
            .collect();
        let (mut seed, mut out) = (7u32, Vec::with_capacity(300_016));
        while out.len() < 300_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.extend_from_slice(list[(seed >> 8) as usize % list.len()].as_bytes());
            out.push(if seed & 0xf == 0 { b'\n' } else { b' ' });
        }
        out
    }

    /// CLB fork: the levels above 4 are `zstd-rust`'s, smaller than `zrip`'s level 4 (what they
    /// were without it) and smaller again at 10 and above, to one frame that records its size and
    /// that `zrip` and the C library decode. The levels up to 4 stay `zrip`'s, byte for byte.
    #[cfg(feature = "zstd-rust")]
    #[test]
    fn levels_above_4_are_written_by_zstd_rust() {
        use zstd_rust::encoding::{CompressionLevel as L, Compressor};
        let a = words();
        let at = |level| pure::compress(&a, level).unwrap();
        for level in [-7, -1, 1, 2, 3, 4] {
            assert_eq!(at(level), zrip::compress(&a, level).unwrap(), "level {level}");
        }
        assert_eq!(at(0), at(3));
        for (levels, want) in [(&[5, 7, 9][..], L::Better), (&[10, 15, 19, 22][..], L::Best)] {
            let bytes = Compressor::new(want).compress(&a);
            for &level in levels {
                assert_eq!(format!("{:?}", pure::rust_level(level)), format!("{want:?}"));
                assert_eq!(at(level), bytes, "level {level}");
            }
        }
        let (four, nine, nineteen) = (at(4), at(9), at(19));
        assert!(nine.len() < four.len(), "level 9: {} bytes, level 4: {}", nine.len(), four.len());
        assert!(nineteen.len() < nine.len(), "level 19: {} bytes, level 9: {}", nineteen.len(), nine.len());
        for frame in [&nine, &nineteen] {
            assert_eq!(pure::decompress(frame, a.len()).unwrap(), a);
            assert_eq!(pure::frames(frame, usize::MAX).unwrap(), a);
            #[cfg(all(
                any(feature = "libzstd-geotiff", feature = "libzstd-ept", feature = "libzstd-geoparquet"),
                not(target_family = "wasm")
            ))]
            {
                assert_eq!(zstd::zstd_safe::get_frame_content_size(frame).unwrap(), Some(a.len() as u64));
                assert_eq!(zstd::bulk::decompress(frame, a.len()).unwrap(), a);
            }
        }
    }

    /// Without the `zstd-rust` feature, as upstream, `zrip` writes the levels above 4 at level 4.
    #[cfg(not(feature = "zstd-rust"))]
    #[test]
    fn levels_above_4_are_zrip_s_level_4_without_zstd_rust() {
        let a = words();
        let four = zrip::compress(&a, 4).unwrap();
        for level in [5, 9, 19, 22] {
            assert_eq!(pure::compress(&a, level).unwrap(), four, "level {level}");
        }
    }

    /// Each format's Zstandard is the C library exactly when its feature is on (natively), as its
    /// output shows: the pure path's level 9 (`zstd-rust`'s `Better`, or `zrip`'s level 4 without
    /// the `zstd-rust` feature) and the C library's level 9 write different bytes.
    #[test]
    fn each_format_uses_the_library_its_feature_names() {
        let a = data(200_000, 13);
        let zrip = pure::compress(&a, 9).unwrap();
        let native = cfg!(not(target_family = "wasm"));
        for (format, compress, c) in [
            ("GeoTIFF", geotiff::compress as fn(&[u8], i32) -> Result<Vec<u8>>, cfg!(feature = "libzstd-geotiff")),
            ("EPT", ept::compress, cfg!(feature = "libzstd-ept")),
            ("GeoParquet", geoparquet::compress, cfg!(feature = "libzstd-geoparquet")),
        ] {
            let out = compress(&a, 9).unwrap();
            assert_eq!(out != zrip, c && native, "{format}");
            assert_eq!(pure::decompress(&out, a.len()).unwrap(), a, "{format}");
        }
    }

    /// Frames that do not record their size (a stream encoder's) are read as a stream, frames
    /// recording it in one call, the two concatenated too, each within its limit or bound.
    #[cfg(all(any(feature = "libzstd-geotiff", feature = "libzstd-ept", feature = "libzstd-geoparquet"), not(target_family = "wasm")))]
    #[test]
    fn frames_with_and_without_their_size_are_read() {
        use zstd::zstd_safe::get_frame_content_size;
        let (a, b) = (data(150_000, 11), data(70_000, 5));
        let unsized_frame = zstd::stream::encode_all(&a[..], 3).unwrap();
        assert_eq!(get_frame_content_size(&unsized_frame).unwrap(), None);
        let sized = libzstd::compress(&b, 3).unwrap();
        assert_eq!(get_frame_content_size(&sized).unwrap(), Some(b.len() as u64));
        let both = [a.clone(), b.clone()].concat();
        for (stream, want) in
            [(unsized_frame.clone(), &a), (sized.clone(), &b), ([unsized_frame.clone(), skippable(5), sized.clone()].concat(), &both)]
        {
            assert_eq!(&libzstd::decompress(&stream, want.len()).unwrap(), want);
            assert_eq!(&libzstd::frames(&stream, want.len()).unwrap(), want);
            assert_eq!(&libzstd::frames(&stream, usize::MAX).unwrap(), want);
            assert_eq!(libzstd::frames(&stream, want.len() - 1).unwrap(), want[..want.len() - 1]);
            let e = libzstd::decompress(&stream, want.len() - 1).unwrap_err();
            assert!(e.to_string().contains("more than"), "{e}");
        }
        // A frame decompressing past the size it records is an error, not a longer vector.
        let mut lying = libzstd::compress(&b, 3).unwrap();
        let (header, fcs) = (lying[4], 4 + 1 + usize::from(lying[4] & 0x20 == 0));
        assert_eq!(header >> 6, 2, "a 4-byte content size field");
        lying[fcs..fcs + 4].copy_from_slice(&(b.len() as u32 - 1).to_le_bytes());
        assert!(libzstd::decompress(&lying, b.len()).is_err());
        assert!(libzstd::frames(&lying, b.len()).is_err());
    }

    /// Each implementation reads what the other writes, frames concatenated across them too.
    #[cfg(all(any(feature = "libzstd-geotiff", feature = "libzstd-ept", feature = "libzstd-geoparquet"), not(target_family = "wasm")))]
    #[test]
    fn zrip_and_libzstd_read_each_other() {
        let (a, b) = (data(200_000, 13), data(50_000, 7));
        for level in [1, 3, 9, 19] {
            let c = libzstd::compress(&a, level).unwrap();
            assert_eq!(pure::decompress(&c, a.len()).unwrap(), a, "level {level}");
        }
        // `zrip`'s levels and, with the `zstd-rust` feature, `zstd-rust`'s above 4 (CLB fork).
        for level in [-7, 1, 3, 4, 9, 19] {
            let p = pure::compress(&a, level).unwrap();
            assert_eq!(libzstd::decompress(&p, a.len()).unwrap(), a, "level {level}");
        }
        let mixed = [libzstd::compress(&a, 9).unwrap(), skippable(3), pure::compress(&b, 9).unwrap()].concat();
        let both = [a, b].concat();
        assert_eq!(pure::decompress(&mixed, both.len()).unwrap(), both);
        assert_eq!(libzstd::decompress(&mixed, both.len()).unwrap(), both);
    }
}
