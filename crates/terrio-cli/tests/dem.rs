//! `terrio dem`'s memory budget through the real executable: taken where the output is written a
//! strip at a time, refused where it is not, as in Python.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

#[test]
fn max_memory_applies_to_strips_only() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (w, h) = (20, 15);
    let values: Vec<f64> = (0..w * h).map(|i| ((i * 31 % 53) as f64) * 2.0).collect();
    let cfg = terrio::RasterConfig { cols: w, rows: h, data_type: terrio::DataType::F32, cell_size: 10., ..Default::default() };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
    r.raster.crs.epsg = Some(32611);
    terrio::write_raster_dataset(&r, d.join("dem.tif").to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(d.join("colors.txt"), "0 0 0 0\n100 255 255 255\n").unwrap();
    for args in [
        &["dem", "slope", "dem.tif", "slope.tif", "--max-memory", "1"][..],
        &["dem", "color-relief", "dem.tif", "colors.txt", "relief.tif", "--max-memory", "1"],
    ] {
        let out = run(args, d);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    for args in [
        &["dem", "slope", "dem.tif", "dem.tif", "--max-memory", "1"][..],
        &["dem", "color-relief", "dem.tif", "colors.txt", "dem.tif", "--max-memory", "1"],
        &["dem", "color-relief", "dem.tif", "colors.txt", "relief.vrt", "--max-memory", "1"],
    ] {
        let out = run(args, d);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--max-memory does not apply"), "{args:?}");
    }
    // The input is unchanged by the refusals.
    assert_eq!(terrio::open_raster(d.join("dem.tif").to_str().unwrap()).unwrap().raster.data.to_f64_vec(), r.raster.data.to_f64_vec());
}
