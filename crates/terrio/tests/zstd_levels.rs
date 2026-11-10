//! Zstandard in GeoTIFFs and COGs with and without the zstd C library (the `libzstd` feature):
//! `ZSTD` and `LERC_ZSTD` written at several levels, and Zstandard chunks of samples the `tiff`
//! crate decodes (bit depths under 8, Float16, WhiteIsZero).
//!
//! The same tests run in both builds:
//!
//! ```sh
//! cargo test -p terrio --test zstd_levels
//! cargo test -p terrio --test zstd_levels --no-default-features --features geotiff,lerc
//! ```
//!
//! With `TERRIO_ZSTD_EXCHANGE` set to a directory, each build also leaves its files there and
//! reads the files the other build left: run one build, then the other, then the first again.

use terrio::formats::RasterDriver;
use terrio::raster::RasterData;
use terrio::{DataType, LazyRaster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

const COLS: usize = 600;
const ROWS: usize = 400;
const LEVELS: [u32; 5] = [1, 3, 9, 19, 22];

/// A terrain: smooth relief with ridges and a little deterministic noise, in metres.
fn terrain() -> Vec<f32> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..COLS * ROWS)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let (x, y) = ((i % COLS) as f32, (i / COLS) as f32);
            let relief = 1200.0 + 400.0 * (x / 97.0).sin() * (y / 61.0).cos() + 90.0 * ((x + 2.0 * y) / 23.0).sin();
            relief + 25.0 * ((x * y) / 4099.0).sin().abs() + ((state >> 56) as f32) / 64.0
        })
        .collect()
}

/// A way of writing the terrain: a name, the driver, the data type and the creation options.
struct Case {
    name: &'static str,
    driver: RasterDriver,
    data_type: DataType,
    options: &'static [(&'static str, &'static str)],
}

fn cases() -> Vec<Case> {
    let case = |name, driver, data_type, options| Case { name, driver, data_type, options };
    let mut cases = vec![
        case("gtiff-zstd-f32-tiles", RasterDriver::GTiff, DataType::F32, &[("COMPRESS", "ZSTD"), ("TILED", "YES"), ("PREDICTOR", "3")][..]),
        case("gtiff-zstd-i16-strips", RasterDriver::GTiff, DataType::I16, &[("COMPRESS", "ZSTD"), ("PREDICTOR", "2")][..]),
        case("gtiff-zstd-u8", RasterDriver::GTiff, DataType::U8, &[("COMPRESS", "ZSTD"), ("TILED", "YES")][..]),
        case("cog-zstd-f32", RasterDriver::Cog, DataType::F32, &[("COMPRESS", "ZSTD")][..]),
        case("cog-zstd-i16", RasterDriver::Cog, DataType::I16, &[("COMPRESS", "ZSTD"), ("PREDICTOR", "YES")][..]),
    ];
    if cfg!(feature = "lerc") {
        cases.extend([
            case("gtiff-lerc-zstd-f32", RasterDriver::GTiff, DataType::F32, &[("COMPRESS", "LERC_ZSTD"), ("TILED", "YES")][..]),
            case("cog-lerc-zstd-i16", RasterDriver::Cog, DataType::I16, &[("COMPRESS", "LERC_ZSTD")][..]),
            case("cog-lerc-zstd-f32-lossy", RasterDriver::Cog, DataType::F32, &[("COMPRESS", "LERC_ZSTD"), ("MAX_Z_ERROR", "0.25")][..]),
        ]);
    }
    cases
}

/// The terrain as `data_type`.
fn dataset(data_type: DataType) -> RasterDataset {
    let z = terrain();
    let data = match data_type {
        DataType::F32 => RasterData::F32(z),
        DataType::I16 => RasterData::I16(z.iter().map(|v| *v as i16).collect()),
        _ => RasterData::U8(z.iter().map(|v| (v / 8.0) as u8).collect()),
    };
    let config = RasterConfig { cols: COLS, rows: ROWS, bands: 1, data_type, ..Default::default() };
    RasterDataset::new(terrio::Raster::from_data_native(config, data).unwrap(), false)
}

fn write(case: &Case, level: u32, path: &std::path::Path) {
    let mut creation_options: Vec<(String, String)> = case.options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    creation_options.push(("LEVEL".into(), level.to_string()));
    let options = RasterWriteOptions { driver: Some(case.driver), creation_options, ..Default::default() };
    terrio::write_raster_dataset(&dataset(case.data_type), path.to_str().unwrap(), &options)
        .unwrap_or_else(|e| panic!("{} at level {level}: {e}", case.name));
}

/// Checks the file holds the terrain as `case` writes it: every sample, or within `MAX_Z_ERROR`.
fn check(case: &Case, path: &std::path::Path) {
    let what = path.display();
    let back = terrio::open_raster(path.to_str().unwrap()).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!((back.raster.cols, back.raster.rows, back.raster.data_type), (COLS, ROWS, case.data_type), "{what}");
    let (got, want) = (back.raster.data.to_f64_vec(), dataset(case.data_type).raster.data.to_f64_vec());
    let tolerance = case.options.iter().find(|(k, _)| *k == "MAX_Z_ERROR").map_or(0.0, |(_, v)| v.parse::<f64>().unwrap());
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    assert!(got.len() == want.len() && worst <= tolerance, "{what}: samples differ by up to {worst}");
}

/// This build's name among the exchanged files.
fn build() -> &'static str {
    if cfg!(feature = "libzstd") { "libzstd" } else { "pure" }
}

#[test]
fn zstd_levels_are_honoured_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    for case in cases() {
        let mut sizes = Vec::new();
        for level in LEVELS {
            let path = dir.path().join(format!("{}-{level}.tif", case.name));
            write(&case, level, &path);
            check(&case, &path);
            sizes.push(std::fs::metadata(&path).unwrap().len());
        }
        // Level 19 is smaller than level 1 (Zstandard over LERC's blobs gains little: no larger),
        // in the pure-Rust build as with the C library; so is level 9 where a predictor (or 8-bit
        // samples) leaves matches to find: noisy Float32 samples as they are gain nothing by it.
        let (first, high) = (sizes[0], sizes[3]);
        println!("{} ({}): {sizes:?} bytes at levels {LEVELS:?}", case.name, build());
        if case.name.contains("lerc") {
            assert!(high <= first, "{} ({}): {sizes:?}", case.name, build());
        } else {
            assert!(high < first, "{} ({}): {sizes:?}", case.name, build());
            let matches = case.data_type == DataType::U8 || case.options.iter().any(|(k, _)| *k == "PREDICTOR");
            assert!(!matches || sizes[2] < first, "{} ({}): {sizes:?}", case.name, build());
        }
    }
}

/// Files written by this build are left in `TERRIO_ZSTD_EXCHANGE`, and the files the builds left
/// there, with the C library and without, are read by this one.
#[test]
fn zstd_files_of_the_other_build_are_read() {
    let Some(root) = std::env::var_os("TERRIO_ZSTD_EXCHANGE").map(std::path::PathBuf::from) else { return };
    let own = root.join(build());
    std::fs::create_dir_all(&own).unwrap();
    for case in cases() {
        for level in LEVELS {
            write(&case, level, &own.join(format!("{}-{level}.tif", case.name)));
        }
    }
    for writer in ["pure", "libzstd"] {
        let dir = root.join(writer);
        let mut read = 0;
        for case in cases() {
            for level in LEVELS {
                let path = dir.join(format!("{}-{level}.tif", case.name));
                if path.exists() {
                    check(&case, &path);
                    read += 1;
                }
            }
        }
        println!("the {} build read {read} files written by the {writer} build", build());
        assert!(writer != build() || read == cases().len() * LEVELS.len());
    }
}

/// A one-band sample layout: a name, the bits of a sample, its format, the photometric
/// interpretation, the predictor and the rows of samples as stored.
struct Layout {
    name: &'static str,
    bits: u16,
    format: u16,
    photometric: u16,
    predictor: u16,
    rows: Vec<Vec<u8>>,
}

/// A TIFF of `w` x `h` one-band samples of `layout`, in strips of 4 rows, Zstandard-compressed
/// or not.
fn tiff(w: u32, h: u32, layout: &Layout, zstd: bool) -> Vec<u8> {
    let Layout { bits, format, photometric, predictor, ref rows, .. } = *layout;
    let strips: Vec<Vec<u8>> = rows
        .chunks(4)
        .map(|r| {
            let raw = r.concat();
            if !zstd {
                return raw;
            }
            // The encoder of builds without the C library, at a high level.
            mzstd::Compressor::new(mzstd::EncoderConfig::default().with_level(mzstd::Level::try_from(19).unwrap()))
                .unwrap()
                .compress(&raw)
                .unwrap()
        })
        .collect();
    let n = strips.len() as u32;
    // (tag, type, count, value): SHORT 3, LONG 4; the strip offsets and byte counts are arrays.
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, w),
        (257, 4, 1, h),
        (258, 3, 1, u32::from(bits)),
        (259, 3, 1, if zstd { 50000 } else { 1 }),
        (262, 3, 1, u32::from(photometric)),
        (273, 4, n, 0),
        (277, 3, 1, 1),
        (278, 4, 1, 4),
        (279, 4, n, 0),
        (317, 3, 1, u32::from(predictor)),
        (339, 3, 1, u32::from(format)),
    ];
    let arrays = 8 + 2 + entries.len() as u32 * 12 + 4;
    let data = arrays + 8 * n;
    entries[5].3 = arrays;
    entries[8].3 = arrays + 4 * n;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut at = data;
    for s in &strips {
        out.extend_from_slice(&at.to_le_bytes());
        at += s.len() as u32;
    }
    for s in &strips {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    }
    for s in &strips {
        out.extend_from_slice(s);
    }
    out
}

/// The data type, stored bits and samples (their bits, NaNs included) of a TIFF in memory.
fn read(bytes: Vec<u8>) -> (DataType, Option<u16>, Vec<u64>) {
    let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
    let all = Window::full(r.width(), r.height());
    let values = r.read_window(all, &WindowOptions { bands: vec![1], overview: None }).unwrap().raster.data.to_f64_vec();
    (r.data_type(), r.sample_bits(), values.into_iter().map(f64::to_bits).collect())
}

/// Zstandard GeoTIFFs of samples the `tiff` crate unpacks, converts or inverts read as the same
/// images stored uncompressed: without the C library these were refused ("built without the
/// `libzstd` feature").
#[test]
fn zstd_chunks_of_samples_the_tiff_crate_decodes_are_read() {
    let (w, h) = (37u32, 10u32);
    let byte = |i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8;
    // Rows of packed bits, of bytes, of 16-bit samples and of Float16 samples.
    let rows =
        |row_bytes: u32| -> Vec<Vec<u8>> { (0..h).map(|y| (0..row_bytes).map(|x| byte(y * row_bytes + x) % 97).collect()).collect() };
    let halves: Vec<Vec<u8>> =
        (0..h).map(|y| (0..w).flat_map(|x| half::f16::from_f32((x * 7 + y) as f32 * 0.25 - 20.0).to_le_bytes()).collect()).collect();
    let layout = |name, bits, format, photometric, predictor, rows| Layout { name, bits, format, photometric, predictor, rows };
    let layouts = [
        layout("1 bit", 1, 1, 1, 1, rows(w.div_ceil(8))),
        layout("1 bit, WhiteIsZero", 1, 1, 0, 1, rows(w.div_ceil(8))),
        layout("2 bits", 2, 1, 1, 1, rows((w * 2).div_ceil(8))),
        layout("4 bits", 4, 1, 1, 1, rows((w * 4).div_ceil(8))),
        layout("UInt8, WhiteIsZero", 8, 1, 0, 1, rows(w)),
        layout("UInt8, WhiteIsZero, predictor 2", 8, 1, 0, 2, rows(w)),
        layout("UInt16, WhiteIsZero, predictor 2", 16, 1, 0, 2, rows(w * 2)),
        layout("Float16", 16, 3, 1, 1, halves.clone()),
        layout("Float16, predictor 3", 16, 3, 1, 3, halves),
        layout("UInt16", 16, 1, 1, 2, rows(w * 2)),
    ];
    for l in &layouts {
        let (plain, zstd) = (read(tiff(w, h, l, false)), read(tiff(w, h, l, true)));
        assert_eq!(zstd, plain, "{} ({})", l.name, build());
        assert_eq!(plain.2.len(), (w * h) as usize, "{}", l.name);
    }
}
