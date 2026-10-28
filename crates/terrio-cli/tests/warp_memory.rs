//! `terrio warp --wm`, the memory of a warp chunk (as `gdalwarp -wm`).

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn values(path: &Path) -> Vec<f64> {
    terrio::open_raster(path.to_str().unwrap()).unwrap().raster.data.to_f64_vec()
}

#[test]
fn chunk_memory_is_set_and_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let (cols, rows) = (300, 200);
    let cfg = terrio::RasterConfig {
        cols,
        rows,
        x_min: 500000.0,
        y_min: 4000000.0,
        cell_size: 30.0,
        data_type: terrio::DataType::F32,
        ..Default::default()
    };
    let v = (0..cols * rows).map(|i| ((i % cols) + 3 * (i / cols)) as f64).collect();
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, v).unwrap(), false);
    r.raster.crs.epsg = Some(32611);
    terrio::write_raster_dataset(&r, d.join("utm.tif").to_str().unwrap(), &Default::default()).unwrap();

    let warp = |out: &str, extra: &[&str]| {
        let mut args = vec!["warp", "utm.tif", out, "--t-srs", "EPSG:4326", "-r", "bilinear"];
        args.extend_from_slice(extra);
        run(&args, d)
    };
    for (out, extra) in [("default.tif", &[][..]), ("64.tif", &["--wm", "64"][..]), ("small.tif", &["--wm", "0.01"][..])] {
        let r = warp(out, extra);
        assert!(r.status.success(), "{extra:?}: {}", String::from_utf8_lossy(&r.stderr));
    }
    // 64 MiB is the default.
    assert_eq!(values(&d.join("64.tif")), values(&d.join("default.tif")));
    // A small chunk memory warps in many chunks, of the same grid.
    assert_eq!(values(&d.join("small.tif")).len(), values(&d.join("default.tif")).len());
    for bad in ["0", "-1", "nan"] {
        assert!(!warp("bad.tif", &["--wm", bad]).status.success(), "{bad}");
    }
}
