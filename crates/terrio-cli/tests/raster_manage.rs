//! Raster-management CLI integration, including preservation on invalid requests.
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).output().unwrap()
}

fn ok(args: &[&str], directory: &Path) {
    let output = run(args, directory);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn invalid_overwrite_preserves_existing_raster() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.tif");
    ok(&["create-raster", "existing.tif", "--size", "8,8", "--burn", "7"], directory.path());
    let original = std::fs::read(&path).unwrap();
    for args in [
        vec!["create-raster", "existing.tif", "--overwrite", "--like", "missing.tif"],
        vec!["create-raster", "existing.tif", "--overwrite", "--size", "0,0"],
        vec!["create-raster", "existing.tif", "--overwrite", "--size", "8,8", "--burn", "1", "--burn", "2"],
    ] {
        assert_eq!(run(&args, directory.path()).status.code(), Some(1));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    ok(&["create-raster", "existing.tif", "--overwrite", "--like", "existing.tif", "--burn", "9"], directory.path());
    let result = terrio::open_raster(path.to_str().unwrap()).unwrap();
    assert!(result.raster.data.to_f64_vec().iter().all(|v| *v == 9.));
}

#[test]
fn invalid_overview_and_move_options_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    ok(&["create-raster", "input.tif", "--size", "32,32", "--burn", "7", "--crs", "EPSG:3857"], directory.path());
    ok(&["overviews", "input.tif", "2", "4"], directory.path());
    for level in ["0", "1"] {
        assert_eq!(run(&["overviews", "input.tif", "--refresh", "--levels", level], directory.path()).status.code(), Some(1));
    }
    for option in ["--stats", "--hist", "--unset-nodata", "--unset-all-metadata"] {
        assert_eq!(run(&["edit-raster", "input.tif", "--t-srs", "EPSG:4326", option], directory.path()).status.code(), Some(2));
    }
    assert_eq!(run(&["create-raster", "bad.png", "--like", "input.tif", "--copy-overviews"], directory.path()).status.code(), Some(1));
    assert!(!directory.path().join("bad.png").exists());
}

#[test]
fn info_shows_the_overview_report_of_area_matched_overviews() {
    let directory = tempfile::tempdir().unwrap();
    // A thin diagonal band of valid values in nodata, 64 x 64.
    let (w, h): (usize, usize) = (64, 64);
    let values: Vec<f64> = (0..w * h).map(|i| if (i % w).abs_diff(i / w) < 3 { (i % 7) as f64 } else { -9999. }).collect();
    let cfg =
        terrio::RasterConfig { cols: w, rows: h, data_type: terrio::DataType::F32, cell_size: 10., nodata: -9999., ..Default::default() };
    let mut source = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), true);
    source.raster.crs.epsg = Some(32611);
    terrio::write_raster_dataset(&source, directory.path().join("band.tif").to_str().unwrap(), &Default::default()).unwrap();
    ok(&["convert", "band.tif", "am.tif", "-f", "COG", "--tile-size", "16", "--co", "OVERVIEW_RESAMPLING=AREA_MATCHED"], directory.path());
    let text = run(&["info", "am.tif"], directory.path());
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("Overview report:\n  OVERVIEW_METHOD=area_matched\n  OVERVIEW_FACTORS=2,4\n"), "{text}");
    let json = run(&["info", "am.tif", "--json"], directory.path());
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let report = json["overview_report"].as_array().unwrap();
    assert_eq!(report.len(), 4);
    assert_eq!(report[3][0], "OVERVIEW_BASE_VALID_PIXELS");
    // Other overviews have no report.
    ok(&["convert", "band.tif", "avg.tif", "-f", "COG", "--tile-size", "16"], directory.path());
    let text = run(&["info", "avg.tif", "--json"], directory.path());
    assert!(serde_json::from_slice::<serde_json::Value>(&text.stdout).unwrap().get("overview_report").is_none());
}
