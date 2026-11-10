//! Zstandard, with the pure-Rust `ruzstd` (the `zstd-rust` crate, CLB's fork of `ruzstd` with the
//! `Default`, `Better` and `Best` levels) or, in native builds, the zstd C library. The choice is
//! made per format, by feature: GeoTIFF/COG chunks use the C library with `libzstd-geotiff` (on by
//! default in this fork), EPT tiles with `libzstd-ept` and GeoParquet pages with
//! `libzstd-geoparquet` (both opt-in; `libzstd` turns on all three). WebAssembly is always
//! `ruzstd`, and so is Zarr. Both read every frame of a stream (skippable frames skipped), and each
//! reads what the other writes.

use std::io::{Error, ErrorKind, Read, Result};

/// The `cfg` of a format's Zstandard: the C library with `$feature` natively, `ruzstd` otherwise.
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
    frames
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
        if let Some(size) = recorded_size(data).filter(|&size| size <= max as u64) {
            return zstd::bulk::decompress(data, size as usize);
        }
        let mut out = Vec::new();
        zstd::stream::read::Decoder::with_buffer(data)?.take(max as u64).read_to_end(&mut out)?;
        Ok(out)
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

/// `ruzstd`: Zarr's, the other formats' without their `libzstd-*` feature, and every format's in
/// WebAssembly.
// Each item is used by the formats without their feature.
#[allow(dead_code)]
pub(crate) mod pure {
    use super::*;
    use ruzstd::decoding::FrameDecoder;
    use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    use ruzstd::encoding::Compressor;
    use std::cell::RefCell;

    thread_local! {
        /// The compressor of this thread, kept from chunk to chunk: its tables (megabytes at the
        /// higher levels) are then allocated once per thread, not once per chunk.
        static COMPRESSOR: RefCell<Option<Compressor>> = const { RefCell::new(None) };
        /// The decoder of this thread, for its tables and buffers.
        static DECODER: RefCell<FrameDecoder> = RefCell::new(FrameDecoder::new());
    }

    /// Compresses `data` at the `ruzstd` level closest to zstd's `level` (see [`ruzstd_level`]),
    /// to a frame that records its size, as the C library's does.
    pub(crate) fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
        let level = ruzstd_level(level);
        Ok(COMPRESSOR.with(|compressor| {
            let mut compressor = compressor.borrow_mut();
            let compressor = compressor.get_or_insert_with(|| Compressor::new(level));
            compressor.set_compression_level(level);
            compressor.compress(data)
        }))
    }

    /// The `ruzstd` level for a zstd level: up to 2 `Fastest` (zstd 1), 0 (zstd's default) and 3
    /// to 5 `Default` (about zstd 3), 6 to 9 `Better` (about zstd 9), from 10 on `Best`.
    pub(crate) fn ruzstd_level(level: i32) -> ruzstd::encoding::CompressionLevel {
        use ruzstd::encoding::CompressionLevel as L;
        match level {
            0 | 3..=5 => L::Default,
            ..=2 => L::Fastest,
            6..=9 => L::Better,
            _ => L::Best,
        }
    }

    /// Decompresses every frame of `data`, to at most `limit` bytes (more is an error).
    pub(crate) fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let out = frames(data, limit.saturating_add(1))?;
        if out.len() > limit {
            return Err(too_long(limit));
        }
        Ok(out)
    }

    /// The first `max` bytes `data` decompresses to, frame after frame, skippable frames skipped.
    ///
    /// A stream whose frames all record their size (as the frames of [`compress`] and of the C
    /// library do), of no more than `max` bytes in all, is decoded at once into a vector of that
    /// size, which takes a third of the time or less; content that does not have the checksum of
    /// its frame is then an error. Other streams are decoded through the window of a stream
    /// decoder, to `max` bytes.
    pub(crate) fn frames(data: &[u8], max: usize) -> Result<Vec<u8>> {
        if let Some(out) = whole(data, max)? {
            return Ok(out);
        }
        stream(data, max)
    }

    /// The content of `data` when its frames record their size, of at most `max` bytes in all,
    /// and decode at once into a vector of that size; `None` for a stream decoder to decode it
    /// (or to say what is wrong with it). Content without the checksum of its frame is an error.
    pub(crate) fn whole(data: &[u8], max: usize) -> Result<Option<Vec<u8>>> {
        // What the headers record is checked against what frames can hold at all: a block of at
        // most 128 KiB takes four bytes or more.
        let most = (data.len() as u64 / 4 + 1).saturating_mul(128 << 10).min(max as u64);
        let Some(size) = ruzstd::decoding::content_size(data).filter(|&size| size <= most) else {
            return Ok(None);
        };
        let mut out = Vec::new();
        if out.try_reserve_exact(size as usize).is_err() {
            return Ok(None);
        }
        out.resize(size as usize, 0);
        match DECODER.with(|decoder| decoder.borrow_mut().decode_all(data, &mut out)) {
            Ok(decoded) => {
                out.truncate(decoded);
                Ok(Some(out))
            }
            Err(e @ FrameDecoderError::ChecksumMismatch { .. }) => Err(Error::new(ErrorKind::InvalidData, format!("Zstandard: {e}"))),
            Err(_) => Ok(None),
        }
    }

    /// The first `max` bytes `data` decompresses to, a frame at a time through a stream decoder.
    fn stream(mut data: &[u8], max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        while !data.is_empty() && out.len() < max {
            match ruzstd::decoding::StreamingDecoder::new(&mut data) {
                Ok(frame) => {
                    frame.take((max - out.len()) as u64).read_to_end(&mut out)?;
                }
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                    data = data
                        .get(length as usize..)
                        .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "Zstandard: truncated skippable frame"))?;
                }
                Err(e) => return Err(Error::new(ErrorKind::InvalidData, format!("Zstandard: {e}"))),
            }
        }
        Ok(out)
    }

    /// [`frames`] through the stream decoder alone, to compare the two ways of decoding.
    #[cfg(test)]
    pub(crate) fn frames_as_a_stream(data: &[u8], max: usize) -> Result<Vec<u8>> {
        stream(data, max)
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
        assert_eq!(ept::decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(geoparquet::decompress(&stream, both.len()).unwrap(), both);
        assert_eq!(geotiff::frames(&stream, usize::MAX).unwrap(), both);
        // At most `max` bytes, the rest not decoded.
        assert_eq!(pure::frames(&stream, 120_000).unwrap(), both[..120_000]);
        // A truncated skippable frame is an error.
        let cut = [pure::compress(&a, 3).unwrap(), skippable(9)[..12].to_vec()].concat();
        assert!(pure::decompress(&cut, usize::MAX).is_err());
    }

    /// Streams whose frames record their size are decoded at once, others through a stream decoder,
    /// to the same bytes, and to their first bytes when they hold more than is asked for.
    #[test]
    fn whole_streams_decode_as_through_a_stream_decoder() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let (a, b) = (data(300_000, 31), data(200_000, 97));
        // The frames of `compress` record their size; a stream compressor's only if they are one block.
        let sized = pure::compress(&a, 9).unwrap();
        assert_eq!(ruzstd::decoding::content_size(&sized), Some(a.len() as u64));
        let unsized_frame = compress_to_vec(&b[..], CompressionLevel::Default);
        assert_eq!(ruzstd::decoding::content_size(&unsized_frame), None);
        let twice = [a.clone(), a.clone()].concat();
        let both = [a.clone(), b.clone()].concat();
        for (stream, want, at_once) in [
            (sized.clone(), &a, true),
            ([sized.clone(), skippable(9), sized.clone()].concat(), &twice, true),
            (unsized_frame.clone(), &b, false),
            ([sized.clone(), unsized_frame.clone()].concat(), &both, false),
        ] {
            assert_eq!(pure::whole(&stream, usize::MAX).unwrap().is_some(), at_once);
            assert_eq!(&pure::frames(&stream, usize::MAX).unwrap(), want);
            assert_eq!(&pure::frames(&stream, want.len()).unwrap(), want);
            assert_eq!(&pure::frames_as_a_stream(&stream, usize::MAX).unwrap(), want);
            // More than is asked for: the first bytes, through the stream decoder.
            assert!(pure::whole(&stream, want.len() - 1).unwrap().is_none());
            assert_eq!(pure::frames(&stream, want.len() - 1).unwrap(), want[..want.len() - 1]);
            assert_eq!(&pure::decompress(&stream, want.len()).unwrap(), want);
            assert!(pure::decompress(&stream, want.len() - 1).is_err());
            // Cut short, either way an error.
            assert!(pure::frames(&stream[..stream.len() - 7], usize::MAX).is_err());
        }
        // A header recording more than its frame could hold is not believed: no vector of that size.
        let mut lying = sized.clone();
        assert_eq!(lying[4] >> 6, 2, "a 4-byte content size field");
        lying[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(pure::whole(&lying, usize::MAX).unwrap().is_none());
    }

    /// Content that does not have the checksum its frame ends with is an error when the frame is
    /// decoded at once, as it is with the C library.
    #[test]
    fn a_wrong_checksum_is_an_error() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let a = data(100_000, 31);
        // The frames of the stream compressor end with a checksum; one block records its size.
        let mut frame = compress_to_vec(&a[..], CompressionLevel::Default);
        assert_eq!(pure::frames(&frame, usize::MAX).unwrap(), a);
        let last = frame.len() - 1;
        frame[last] ^= 1;
        let e = pure::frames(&frame, usize::MAX).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
    }

    #[test]
    fn unlimited_output_limit_does_not_overflow() {
        let input = b"a finite frame with an unlimited caller policy";
        let compressed = pure::compress(input, 1).unwrap();
        assert_eq!(geoparquet::decompress(&compressed, usize::MAX).unwrap(), input);
        assert_eq!(pure::decompress(&compressed, usize::MAX).unwrap(), input);
    }

    #[test]
    fn small_streams_expanding_past_the_limit_are_errors() {
        // 64 MiB of zeros in a few kilobytes.
        let bomb = pure::compress(&vec![0; 64 << 20], 3).unwrap();
        assert!(bomb.len() < 1 << 20);
        for e in [pure::decompress(&bomb, 1 << 20).unwrap_err(), geoparquet::decompress(&bomb, 1 << 20).unwrap_err()] {
            assert!(e.to_string().contains("more than 1048576 bytes"), "{e}");
        }
        assert_eq!(geoparquet::decompress(&bomb, 64 << 20).unwrap().len(), 64 << 20);
    }

    /// The bound of ZSTD GeoTIFF chunks (`frames`, given the chunk's size): a stream expanding far
    /// past it is decoded to that many bytes and no further, by the implementation in use and by
    /// `ruzstd`. The values decoded are the same either way: only the length shows the bound.
    #[test]
    fn frames_decode_no_further_than_their_bound() {
        // 64 MiB of zeros, then a second frame, in a few kilobytes.
        let bomb = [pure::compress(&vec![0; 64 << 20], 3).unwrap(), pure::compress(&[1; 100], 3).unwrap()].concat();
        assert!(bomb.len() < 1 << 20);
        for max in [0, 1, 4096, 1 << 20] {
            for (decoder, out) in [("in use", geotiff::frames(&bomb, max).unwrap()), ("ruzstd", pure::frames(&bomb, max).unwrap())] {
                assert_eq!(out.len(), max, "{decoder}");
                assert!(out.iter().all(|&b| b == 0), "{decoder}");
            }
        }
        assert_eq!(geotiff::frames(&bomb, usize::MAX).unwrap().len(), (64 << 20) + 100);
    }

    #[test]
    fn zstd_levels_map_to_the_nearest_ruzstd_level() {
        use ruzstd::encoding::CompressionLevel as L;
        let level = |l| format!("{:?}", pure::ruzstd_level(l));
        for (zstd, want) in [
            (i32::MIN, L::Fastest),
            (-5, L::Fastest),
            (0, L::Default),
            (1, L::Fastest),
            (2, L::Fastest),
            (3, L::Default),
            (5, L::Default),
            (6, L::Better),
            (9, L::Better),
            (10, L::Best),
            (22, L::Best),
            (i32::MAX, L::Best),
        ] {
            assert_eq!(level(zstd), format!("{want:?}"), "zstd level {zstd}");
        }
        // Every level round-trips, and compresses.
        let a = data(300_000, 13);
        for l in [1, 3, 9, 19] {
            let c = pure::compress(&a, l).unwrap();
            assert!(c.len() < a.len() / 4, "level {l}: {} bytes", c.len());
            assert_eq!(pure::decompress(&c, a.len()).unwrap(), a, "level {l}");
        }
    }

    /// Each format's Zstandard is the C library exactly when its feature is on (natively), as its
    /// output shows: `ruzstd`'s `Better` and the C library's level 9 write different bytes.
    #[test]
    fn each_format_uses_the_library_its_feature_names() {
        let a = data(200_000, 13);
        let ruzstd = pure::compress(&a, 9).unwrap();
        let native = cfg!(not(target_family = "wasm"));
        for (format, compress, c) in [
            ("GeoTIFF", geotiff::compress as fn(&[u8], i32) -> Result<Vec<u8>>, cfg!(feature = "libzstd-geotiff")),
            ("EPT", ept::compress, cfg!(feature = "libzstd-ept")),
            ("GeoParquet", geoparquet::compress, cfg!(feature = "libzstd-geoparquet")),
        ] {
            let out = compress(&a, 9).unwrap();
            assert_eq!(out != ruzstd, c && native, "{format}");
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
    fn ruzstd_and_libzstd_read_each_other() {
        let (a, b) = (data(200_000, 13), data(50_000, 7));
        for level in [1, 3, 9, 19] {
            let c = libzstd::compress(&a, level).unwrap();
            assert_eq!(pure::decompress(&c, a.len()).unwrap(), a, "level {level}");
        }
        // Every `ruzstd` level (Fastest, Default, Better, Best).
        for level in [1, 3, 9, 19] {
            let p = pure::compress(&a, level).unwrap();
            assert_eq!(libzstd::decompress(&p, a.len()).unwrap(), a, "level {level}");
        }
        let mixed = [libzstd::compress(&a, 9).unwrap(), skippable(3), pure::compress(&b, 9).unwrap()].concat();
        let both = [a, b].concat();
        assert_eq!(pure::decompress(&mixed, both.len()).unwrap(), both);
        assert_eq!(libzstd::decompress(&mixed, both.len()).unwrap(), both);
    }
}
