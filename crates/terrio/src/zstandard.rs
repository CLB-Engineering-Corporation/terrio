//! Zstandard: with the zstd C library with the `libzstd` feature, in native builds; otherwise in
//! pure Rust, with `mzstd` (a port of the C library: zstd's levels, and the frames the C library
//! writes at them). Both read every frame of a stream (skippable frames skipped), and each reads
//! what the other writes. LERC's Zstandard wrapper, Zarr chunks and PMTiles payloads are read in
//! pure Rust in every build.

use std::io::{Error, ErrorKind, Result};

// `decompress` reads EPT tiles (the `pointcloud` feature).
#[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
#[cfg_attr(not(feature = "pointcloud"), allow(unused_imports))]
pub(crate) use libzstd::{compress, decompress};
#[cfg(not(all(feature = "libzstd", not(target_family = "wasm"))))]
#[cfg_attr(not(feature = "pointcloud"), allow(unused_imports))]
pub(crate) use pure::{compress, decompress};

/// The error of a stream decompressing to more than `limit` bytes.
fn too_long(limit: usize) -> Error {
    Error::new(ErrorKind::InvalidData, format!("Zstandard: decompresses to more than {limit} bytes"))
}

#[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
mod libzstd {
    use super::*;
    use std::io::Read;

    /// Compresses `data` at `level`.
    pub(crate) fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
        zstd::bulk::compress(data, level)
    }

    /// Decompresses every frame of `data`, to at most `limit` bytes (more is an error, rather
    /// than an allocation of whatever a small stream expands to).
    #[cfg_attr(not(feature = "pointcloud"), allow(dead_code))]
    pub(crate) fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        zstd::stream::read::Decoder::with_buffer(data)?.take((limit as u64).saturating_add(1)).read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(too_long(limit));
        }
        Ok(out)
    }
}

/// Pure Rust, used without `libzstd`, and in WebAssembly.
#[cfg_attr(all(feature = "libzstd", not(target_family = "wasm")), allow(dead_code))]
pub(crate) mod pure {
    use super::*;

    /// Compresses `data` at `level` as the zstd C library does: levels 1 to 22 (0 is the default
    /// level, 3; negative levels are its fast ones; a level past either end is the nearest).
    ///
    /// A debug build for a 32-bit target (a development WebAssembly build) writes `ruzstd`'s
    /// fastest level instead, about zstd's 1: `mzstd` 0.2.12 adds to indexes that wrap past 32
    /// bits (the range check after them then rejects the candidate, as it is meant to), which
    /// panics with the overflow checks of such a build.
    pub(crate) fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
        if cfg!(all(target_pointer_width = "32", debug_assertions)) {
            return Ok(ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest));
        }
        let failed = |e: &dyn std::fmt::Display| Error::other(format!("Zstandard: {e}"));
        let level = mzstd::Level::try_from(level.clamp(mzstd::Level::MIN.get(), mzstd::Level::MAX.get())).map_err(|e| failed(&e))?;
        let mut encoder = mzstd::Compressor::new(mzstd::EncoderConfig::default().with_level(level)).map_err(|e| failed(&e))?;
        encoder.compress(data).map_err(|e| failed(&e))
    }

    /// Decompresses every frame of `data`, to at most `limit` bytes (more is an error).
    #[cfg_attr(not(feature = "pointcloud"), allow(dead_code))]
    pub(crate) fn decompress(data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let out = frames(data, limit.saturating_add(1))?;
        if out.len() > limit {
            return Err(too_long(limit));
        }
        Ok(out)
    }

    /// The first `max` bytes `data` decompresses to, frame after frame, skippable frames skipped.
    /// A stream of no more than `max` bytes is decoded whole, into the vector returned (its frames
    /// and their checksums verified); a longer one is decoded no further than `max`.
    ///
    /// A debug build for a 32-bit target decodes with `ruzstd` (see [`compress`]).
    pub(crate) fn frames(data: &[u8], max: usize) -> Result<Vec<u8>> {
        if cfg!(all(target_pointer_width = "32", debug_assertions)) {
            return terrio_crs::tiff_zstd::frames(data, max);
        }
        let mut out = Vec::new();
        if data.is_empty() || max == 0 {
            return Ok(out);
        }
        let failed = |e: mzstd::DecodeError| {
            let message = format!("Zstandard: {e}");
            Error::new(Error::from(e).kind(), message)
        };
        let limits = mzstd::DecodeLimits::DEFAULT.with_max_output_bytes(Some(max as u64));
        match mzstd::Decompressor::new(limits.into()).decompress_into(data, &mut out) {
            Ok(_) => Ok(out),
            // Longer than `max`: its first `max` bytes, block after block.
            Err(mzstd::DecodeError::OutputLimitExceeded { .. }) => {
                use std::io::Read;
                let mut decoder = mzstd::Decompressor::default();
                decoder.reader(data).take(max as u64).read_to_end(&mut out).map_err(|e| Error::new(e.kind(), format!("Zstandard: {e}")))?;
                Ok(out)
            }
            Err(e) => Err(failed(e)),
        }
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
        assert_eq!(pure::decompress(&compressed, usize::MAX).unwrap(), input);
    }

    #[test]
    fn small_streams_expanding_past_the_limit_are_errors() {
        // 64 MiB of zeros in a few kilobytes.
        let bomb = pure::compress(&vec![0; 64 << 20], 3).unwrap();
        assert!(bomb.len() < 1 << 20);
        for e in [pure::decompress(&bomb, 1 << 20).unwrap_err(), decompress(&bomb, 1 << 20).unwrap_err()] {
            assert!(e.to_string().contains("more than 1048576 bytes"), "{e}");
        }
        assert_eq!(decompress(&bomb, 64 << 20).unwrap().len(), 64 << 20);
    }

    /// Each implementation reads what the other writes, frames concatenated across them too.
    #[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
    #[test]
    fn pure_rust_and_libzstd_read_each_other() {
        let (a, b) = (data(200_000, 13), data(50_000, 7));
        for level in [1, 3, 9, 19] {
            let c = libzstd::compress(&a, level).unwrap();
            assert_eq!(pure::decompress(&c, a.len()).unwrap(), a, "level {level}");
        }
        let p = pure::compress(&a, 9).unwrap();
        assert_eq!(libzstd::decompress(&p, a.len()).unwrap(), a);
        let mixed = [libzstd::compress(&a, 9).unwrap(), skippable(3), pure::compress(&b, 9).unwrap()].concat();
        let both = [a, b].concat();
        assert_eq!(pure::decompress(&mixed, both.len()).unwrap(), both);
        assert_eq!(libzstd::decompress(&mixed, both.len()).unwrap(), both);
    }

    /// Pseudo-random bytes (xorshift): incompressible.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// A `size` x `size` terrain: smooth relief with ridges and a little noise, in metres.
    fn terrain(size: usize) -> Vec<f32> {
        let rough = noise(size * size, 99);
        (0..size * size)
            .map(|i| {
                let (x, y) = ((i % size) as f32, (i / size) as f32);
                let relief = 1200.0 + 400.0 * (x / 97.0).sin() * (y / 61.0).cos() + 90.0 * ((x + 2.0 * y) / 23.0).sin();
                relief + 25.0 * ((x * y) / 4099.0).sin().abs() + f32::from(rough[i]) / 64.0
            })
            .collect()
    }

    /// The chunks a GeoTIFF writer compresses, of a 256 x 256 terrain tile: Float32 samples as
    /// they are and with the floating-point predictor (byte planes, differenced), Int16 samples
    /// with the horizontal predictor, and 8-bit samples.
    fn raster_tiles() -> Vec<(&'static str, Vec<u8>)> {
        let size = 256;
        let z = terrain(size);
        let float: Vec<u8> = z.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut planes = Vec::with_capacity(float.len());
        for row in z.chunks(size) {
            let mut bytes: Vec<u8> = (0..4).flat_map(|b| row.iter().map(move |v| v.to_be_bytes()[b])).collect();
            for i in (1..bytes.len()).rev() {
                bytes[i] = bytes[i].wrapping_sub(bytes[i - 1]);
            }
            planes.extend(bytes);
        }
        let int16: Vec<u8> = z
            .chunks(size)
            .flat_map(|row| row.iter().enumerate().map(|(i, v)| (*v as i16).wrapping_sub(if i == 0 { 0 } else { row[i - 1] as i16 })))
            .flat_map(i16::to_le_bytes)
            .collect();
        let byte: Vec<u8> = z.iter().map(|v| (v / 8.0) as u8).collect();
        vec![("Float32", float), ("Float32 predictor 3", planes), ("Int16 predictor 2", int16), ("Byte", byte)]
    }

    /// The inputs of the round-trip tests: empty, tiny, incompressible, highly repetitive, and
    /// raster tiles.
    fn inputs() -> Vec<(&'static str, Vec<u8>)> {
        let mut inputs = vec![
            ("empty", Vec::new()),
            ("one byte", vec![7]),
            ("tiny", b"terrio".to_vec()),
            ("short text", b"a tile, a tile, a tile of tiles".to_vec()),
            ("noise", noise(150_000, 5)),
            ("zeros", vec![0; 300_000]),
            ("one value", vec![0xA5; 70_001]),
            ("a period of 8", b"abcdefgh".repeat(20_000)),
            ("ramp", data(200_000, 31)),
        ];
        inputs.extend(raster_tiles());
        inputs
    }

    /// Frames compressed in pure Rust at every level, 1 to 22, and at levels outside them,
    /// decompress to their input with `mzstd`, `ruzstd` and the zstd C library.
    #[test]
    fn pure_rust_frames_of_every_level_decompress() {
        for (name, input) in inputs() {
            for level in (1..=22).chain([0, -1, -7, 23, 100, i32::MIN, i32::MAX]) {
                let frame = pure::compress(&input, level).unwrap();
                assert_eq!(pure::decompress(&frame, input.len()).unwrap(), input, "{name}, level {level}: mzstd");
                assert_eq!(terrio_crs::tiff_zstd::frames(&frame, usize::MAX).unwrap(), input, "{name}, level {level}: ruzstd");
                #[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
                assert_eq!(libzstd::decompress(&frame, input.len()).unwrap(), input, "{name}, level {level}: libzstd");
            }
        }
    }

    /// The level is honoured: raster tiles shrink from level 1 to 19, and are no larger at 9 than
    /// at 1 (noisy Float32 samples without a predictor are the same size at both, with the C
    /// library too), where `ruzstd`'s only level made them all the size of level 1.
    #[test]
    fn higher_levels_compress_raster_tiles_further() {
        for (name, tile) in raster_tiles() {
            let size = |level| pure::compress(&tile, level).unwrap().len();
            let (l1, l9, l19) = (size(1), size(9), size(19));
            assert!(l19 < l1 && l19 <= l9 && l9 <= l1 && l1 < tile.len(), "{name}: {l1}, {l9} and {l19} bytes of {}", tile.len());
            println!("{name}: {l1}, {l9} and {l19} bytes at levels 1, 9 and 19, of {}", tile.len());
            // No larger at level 1 than what `ruzstd` writes.
            let fastest = ruzstd::encoding::compress_to_vec(&tile[..], ruzstd::encoding::CompressionLevel::Fastest).len();
            assert!(l1 <= fastest, "{name}: {l1} bytes at level 1, {fastest} with ruzstd");
            assert_eq!(size(0), size(3), "{name}: level 0 is the default level");
            assert_eq!(size(23), size(22), "{name}: levels past 22 are 22");
        }
    }

    /// Frames are the size of the zstd C library's at the same level: within 1 % (and 16 bytes),
    /// at every level, on every input. With zstd 1.5.7 they are the same bytes, which is not
    /// required: another version of the C library may choose other matches.
    #[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
    #[test]
    fn pure_rust_frames_are_the_size_of_libzstd_frames() {
        let (mut frames, mut identical) = (0, 0);
        for (name, input) in inputs() {
            for level in 1..=22 {
                let (ours, theirs) = (pure::compress(&input, level).unwrap(), libzstd::compress(&input, level).unwrap());
                let slack = theirs.len() / 100 + 16;
                assert!(
                    ours.len() <= theirs.len() + slack && theirs.len() <= ours.len() + slack,
                    "{name}, level {level}: {} bytes, {} with libzstd",
                    ours.len(),
                    theirs.len()
                );
                frames += 1;
                identical += usize::from(ours == theirs);
            }
        }
        println!("{identical} of {frames} frames are byte-identical to libzstd's");
    }

    /// 512 x 512 chunks of a TIFF (at most 8, spread over it) as a GeoTIFF writer compresses
    /// them: their samples as they are, or differenced along rows as TIFF's predictor 2
    /// (integers) or 3 (floating point). `None` without the file.
    #[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
    fn real_chunks(path: &std::path::Path, predictor: bool) -> Option<(String, Vec<Vec<u8>>)> {
        use tiff::decoder::{Decoder, Limits};
        use tiff::tags::Tag;
        let reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
        let mut dec = Decoder::new(reader).ok()?.with_limits(Limits::unlimited());
        let (w, h) = dec.dimensions().ok()?;
        let (w, h) = (w as usize, h as usize);
        let bits = usize::from(*dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample).ok()??.first()?);
        let format = dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat).ok()?.and_then(|f| f.first().copied()).unwrap_or(1);
        let float = format == 3;
        let mut image = dec.read_image().ok()?;
        let pixels = image.as_buffer(0).as_bytes().to_vec();
        let (size, n) = (bits / 8, 512);
        let pixel = pixels.len() / (w * h);
        let spp = pixel / size;
        let (across, down) = (w / n, h / n);
        let step = (across * down).div_ceil(8).max(1);
        // The bytes of a native-endian sample, least significant first.
        let le = |b: usize| if cfg!(target_endian = "little") { b } else { size - 1 - b };
        let mut out = Vec::new();
        for t in (0..across * down).step_by(step) {
            let mut chunk = Vec::with_capacity(n * n * pixel);
            for r in 0..n {
                let at = ((t / across * n + r) * w + t % across * n) * pixel;
                let row = &pixels[at..at + n * pixel];
                if !predictor {
                    chunk.extend_from_slice(row);
                } else if float {
                    // Byte planes, most significant first, differenced.
                    let mut planes: Vec<u8> = (0..size).rev().flat_map(|b| row.chunks(size).map(move |v| v[le(b)])).collect();
                    for i in (spp..planes.len()).rev() {
                        planes[i] = planes[i].wrapping_sub(planes[i - spp]);
                    }
                    chunk.extend(planes);
                } else {
                    // Each sample less the same sample of the pixel to its left.
                    let mut row = row.to_vec();
                    for i in (pixel..row.len()).step_by(size).rev() {
                        let mut borrow = 0;
                        for b in 0..size {
                            let d = u16::from(row[i + le(b)]).wrapping_sub(u16::from(row[i + le(b) - pixel])).wrapping_sub(borrow);
                            borrow = (d >> 8) & 1;
                            row[i + le(b)] = d as u8;
                        }
                    }
                    chunk.extend(row);
                }
            }
            out.push(chunk);
        }
        let predictor = if !predictor {
            ""
        } else if float {
            ", predictor 3"
        } else {
            ", predictor 2"
        };
        let file = path.file_name()?.to_string_lossy();
        Some((format!("{file}: {}{bits} x {spp}{predictor}", ["UInt", "Int", "Float"][usize::from(format.clamp(1, 3)) - 1]), out))
    }

    /// A size and speed comparison with the zstd C library on chunks of real rasters, printed as
    /// a table (see docs/testing.md):
    /// `TERRIO_TEST_DATA=... cargo test --release -p terrio --lib zstandard -- --ignored --nocapture`.
    #[cfg(all(feature = "libzstd", not(target_family = "wasm")))]
    #[test]
    #[ignore = "a benchmark: needs TERRIO_TEST_DATA and a release build"]
    fn compares_with_libzstd_on_real_rasters() {
        use std::time::Instant;
        type Compress = fn(&[u8], i32) -> Result<Vec<u8>>;
        type Decompress = fn(&[u8], usize) -> Result<Vec<u8>>;
        let Some(dir) = std::env::var_os("TERRIO_TEST_DATA").map(std::path::PathBuf::from) else {
            println!("TERRIO_TEST_DATA is not set");
            return;
        };
        let mut sets = Vec::new();
        for file in ["usgs_dem.tif", "srtm90.tif", "landsat.tif"] {
            sets.extend([real_chunks(&dir.join(file), false), real_chunks(&dir.join(file), true)].into_iter().flatten());
        }
        assert!(!sets.is_empty(), "no usgs_dem.tif, srtm90.tif or landsat.tif in TERRIO_TEST_DATA");
        // The frames written and the megabytes compressed per second.
        let run = |compress: Compress, chunks: &[Vec<u8>], level: i32| -> (Vec<Vec<u8>>, f64) {
            let started = Instant::now();
            let frames = chunks.iter().map(|c| compress(c, level).unwrap()).collect();
            (frames, chunks.iter().map(Vec::len).sum::<usize>() as f64 / 1e6 / started.elapsed().as_secs_f64())
        };
        println!(
            "| Chunks | Level | libzstd bytes | Pure Rust bytes | Size ratio | Identical frames | libzstd MB/s | Pure Rust MB/s | Speed ratio |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        for (name, chunks) in &sets {
            let raw: usize = chunks.iter().map(Vec::len).sum();
            let name = format!("{name}, {} x {:.2} MB", chunks.len(), raw as f64 / chunks.len() as f64 / 1e6);
            for level in [1, 3, 9, 19, 22] {
                // The faster of three runs each, alternating.
                let (mut c, mut p) = ((Vec::new(), 0.0f64), (Vec::new(), 0.0f64));
                for _ in 0..3 {
                    let ((cs, ct), (ps, pt)) = (run(libzstd::compress, chunks, level), run(pure::compress, chunks, level));
                    (c, p) = ((cs, c.1.max(ct)), (ps, p.1.max(pt)));
                }
                let bytes = |frames: &[Vec<u8>]| frames.iter().map(Vec::len).sum::<usize>();
                let same = c.0.iter().zip(&p.0).filter(|(a, b)| a == b).count();
                let (size, speed) = (bytes(&p.0) as f64 / bytes(&c.0) as f64, p.1 / c.1);
                println!(
                    "| {name} | {level} | {} | {} | {size:.4} | {same} of {} | {:.1} | {:.1} | {speed:.2} |",
                    bytes(&c.0),
                    bytes(&p.0),
                    chunks.len(),
                    c.1,
                    p.1
                );
            }
            // The level 9 frames, decompressed by the two decoders: the fastest of three runs.
            let frames: Vec<Vec<u8>> = chunks.iter().map(|c| pure::compress(c, 9).unwrap()).collect();
            let time = |decompress: Decompress| {
                let once = || {
                    let started = Instant::now();
                    for (f, c) in frames.iter().zip(chunks) {
                        assert_eq!(decompress(f, c.len()).unwrap().len(), c.len());
                    }
                    raw as f64 / 1e6 / started.elapsed().as_secs_f64()
                };
                (0..3).map(|_| once()).fold(0.0f64, f64::max)
            };
            let (c, p) = (time(libzstd::decompress), time(pure::decompress));
            println!("| {name}, decompressed | 9 | | | | | {c:.0} | {p:.0} | {:.2} |", p / c);
            // And by `ruzstd`, the decoder of terrio-crs's grids.
            let r = time(terrio_crs::tiff_zstd::frames);
            println!("| {name}, decompressed with ruzstd | 9 | | | | | {c:.0} | {r:.0} | {:.2} |", r / c);
        }
    }
}
