//! `terrio translate` and `terrio convert` with `--co LERC_ENCODER=`: the profile reaches the
//! GeoTIFF and COG writers, GDAL's being the default, a value other than GDAL or COMPACT is
//! refused, and `--co LERC_VERSION=` warns as GDAL 3.12.2 and 3.13.3 do, changing nothing.

use std::path::Path;
use std::process::{Command, Output};
use terrio::{DataType, Raster, RasterConfig, RasterDataset};

fn terrio(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(dir).args(args).output().unwrap()
}

/// Writes `src.tif`: three UInt16 bands of 300 x 200, a surface with noise. Pixel-interleaved
/// integer data is where the compact profile's blobs (Lerc2 v5 difference tiles) differ from GDAL's.
fn source(dir: &Path) -> Vec<f64> {
    let (cols, rows) = (300, 200);
    let mut x = 7u64;
    let values: Vec<f64> = (0..cols * rows * 3)
        .map(|i| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (c, r, b) = ((i % cols) as f64, (i / cols % rows) as f64, (i / cols / rows) as f64);
            let v = 0.013 * c + 0.007 * r + 2.0 * (c / 17.0 + b).sin() * (r / 23.0).cos();
            (32768.0 + v * 4000.0 + (x >> 61) as f64).round()
        })
        .collect();
    let config = RasterConfig { cols, rows, bands: 3, data_type: DataType::U16, ..Default::default() };
    let ds = RasterDataset::new(Raster::from_data(config, values.clone()).unwrap(), false);
    terrio::write_raster_dataset(&ds, dir.join("src.tif").to_str().unwrap(), &Default::default()).unwrap();
    values
}

#[test]
fn translate_and_convert_pass_lerc_encoder_to_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let values = source(dir.path());
    for command in ["translate", "convert"] {
        for format in ["GTiff", "COG"] {
            let write = |out: &str, extra: &[&str]| {
                let mut args = vec![command, "src.tif", out, "--format", format, "--co", "COMPRESS=LERC"];
                args.extend(extra.iter().flat_map(|o| ["--co", o]));
                let result = terrio(dir.path(), &args);
                assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
                let back = terrio::open_raster(dir.path().join(out).to_str().unwrap()).unwrap();
                assert!(back.raster.data.to_f64_vec() == values, "{args:?}: lossless");
                (std::fs::read(dir.path().join(out)).unwrap(), String::from_utf8_lossy(&result.stderr).into_owned())
            };
            let (default, _) = write("default.tif", &[]);
            let (gdal, _) = write("gdal.tif", &["LERC_ENCODER=gdal"]);
            let (compact, _) = write("compact.tif", &["LERC_ENCODER=COMPACT"]);
            assert!(default == gdal, "{command} {format}: GDAL's blobs are the default");
            assert!(compact.len() < gdal.len(), "{command} {format}: COMPACT reaches the writer: {} >= {}", compact.len(), gdal.len());
            // GDAL has no LERC_VERSION creation option: it warns and writes what it would have.
            let (versioned, stderr) = write("versioned.tif", &["LERC_VERSION=3"]);
            assert!(versioned == default, "{command} {format}");
            assert!(stderr.contains(&format!("warning: driver {format} does not support creation option LERC_VERSION")), "{stderr}");

            let result = terrio(
                dir.path(),
                &[command, "src.tif", "bad.tif", "--format", format, "--co", "COMPRESS=LERC", "--co", "LERC_ENCODER=fast"],
            );
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(
                !result.status.success() && stderr.contains("invalid LERC_ENCODER 'fast' (GDAL, COMPACT)"),
                "{command} {format}: {stderr}"
            );
        }
    }
}
