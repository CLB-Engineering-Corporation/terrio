//! Point cloud reprojection between CRSs with heights: `z` is transformed with `x` and `y`, as
//! PDAL's `filters.reprojection` transforms it, and the vertical CRS a LAS file declares is part
//! of the CRS reprojected from.
//!
//! The expected values are PROJ 9.9.0's (pyproj 3.8.0, `always_xy`). The geoid model cases need
//! `us_noaa_g2018u0.tif` in the `grids` directory of `TERRIO_TEST_DATA` (see
//! `scripts/fetch_test_data.sh`) and pass without checking anything when it is missing.

use terrio::crs::SpatialRef;
use terrio::pointcloud::{self, ConvertOptions, Dimension, PointCloud, PointWriteOptions, Values};

/// Points with `x`, `y` and `z` (and a classification, so that formats 1 and 6 apply).
fn cloud(points: &[(f64, f64, f64)], crs: &str) -> PointCloud {
    let dim = |name: &str, f: fn(&(f64, f64, f64)) -> f64| Dimension::new(name, Values::F64(points.iter().map(f).collect()));
    let class = Dimension::new("classification", Values::U8(vec![2; points.len()]));
    PointCloud::new(vec![dim("x", |p| p.0), dim("y", |p| p.1), dim("z", |p| p.2), class], SpatialRef::parse(crs).unwrap())
}

/// Coordinates stored to the centimetre, heights to the millimetre (kept by reprojection).
const MM: [f64; 3] = [0.01, 0.01, 0.001];

fn zs(path: &str) -> Vec<f64> {
    let back = pointcloud::read_points(path, &Default::default()).unwrap();
    (0..back.len()).map(|i| back.dim("z").unwrap().values.get_f64(i)).collect()
}

fn convert(src: &str, dst: &str, t_srs: &str) {
    let options = ConvertOptions { target_srs: Some(SpatialRef::parse(t_srs).unwrap()), ..Default::default() };
    pointcloud::convert(src, dst, &options).unwrap();
}

#[test]
fn reprojection_converts_vertical_units_of_the_file_crs() {
    let dir = tempfile::tempdir().unwrap();
    // NAD83(2011) / Texas South Central (ftUS) + NAVD88 height (ftUS), as Texas lidar is delivered,
    // in GeoKeys (LAS 1.2, format 1) and in a WKT record (LAS 1.4, format 6).
    for (name, format) in [("ft.las", 1), ("ft.laz", 1), ("ft14.laz", 6)] {
        let src = dir.path().join(name);
        let src = src.to_str().unwrap();
        let write = PointWriteOptions {
            point_format: Some(format),
            minor_version: Some(if format == 6 { 4 } else { 2 }),
            scale: Some(MM),
            ..Default::default()
        };
        pointcloud::write_points(&cloud(&[(3000000.0, 13800000.0, 300.0), (3000100.0, 13800100.0, -12.5)], "EPSG:6588+6360"), src, &write)
            .unwrap();
        // The file declares both CRSs: GeoKeys read as the compound CRS (as GDAL reads them), a
        // WKT record as its horizontal CRS and the vertical CRS beside it.
        let r = pointcloud::open_points(src).unwrap();
        assert_eq!(r.vertical_crs().0, Some(6360), "{name}");
        assert!(r.crs().epsg == Some(6588) || r.crs().same_as(&SpatialRef::parse("EPSG:6588+6360").unwrap()), "{name}: {:?}", r.crs());

        // To NAVD88 height in metres: 300 ftUS is 91.44018288036575 m (PROJ).
        let out = dir.path().join(format!("m_{name}"));
        let out = out.to_str().unwrap();
        convert(src, out, "EPSG:6588+5703");
        let z = zs(out);
        assert!((z[0] - 91.44018288036575).abs() < 1e-3 && (z[1] - -12.5 * 0.3048006096012192).abs() < 1e-3, "{name}: {z:?}");
        let r = pointcloud::open_points(out).unwrap();
        assert_eq!(r.vertical_crs().0, Some(5703), "{name}");
        assert!(r.crs().epsg == Some(6588) || r.crs().same_as(&SpatialRef::parse("EPSG:6588+5703").unwrap()), "{name}: {:?}", r.crs());

        // To a CRS without heights, heights are kept (and so is their unit), as by PDAL.
        let flat = dir.path().join(format!("flat_{name}"));
        convert(src, flat.to_str().unwrap(), "EPSG:6588");
        assert!((zs(flat.to_str().unwrap())[0] - 300.0).abs() < 1e-9, "{name}");
    }

    // In memory, with the vertical CRS of the header's WKT record.
    let src = dir.path().join("ft14.laz");
    let mut points = pointcloud::read_points(src.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(points.crs.epsg, Some(6588));
    points.reproject(&SpatialRef::parse("EPSG:6588+5703").unwrap()).unwrap();
    let z = points.dim("z").unwrap().values.get_f64(0);
    assert!((z - 91.44018288036575).abs() < 1e-9, "{z}");

    // A CRS of the points that has the vertical CRS itself.
    let mut points = cloud(&[(3000000.0, 13800000.0, 300.0)], "EPSG:6588+6360");
    points.reproject(&SpatialRef::parse("EPSG:6588+5703").unwrap()).unwrap();
    assert!((points.dim("z").unwrap().values.get_f64(0) - 91.44018288036575).abs() < 1e-9);
}

#[test]
fn reprojection_into_heights_from_none_keeps_them_and_warns() {
    let _ = terrio::warnings::take();
    let mut points = cloud(&[(3000000.0, 13800000.0, 300.0)], "EPSG:6588");
    points.reproject(&SpatialRef::parse("EPSG:6588+5703").unwrap()).unwrap();
    assert_eq!(points.dim("z").unwrap().values.get_f64(0), 300.0);
    let warnings = terrio::warnings::take();
    assert!(warnings.iter().any(|w| w.contains("no vertical CRS")), "{warnings:?}");
    // No warning between CRSs without heights.
    let mut points = cloud(&[(3000000.0, 13800000.0, 300.0)], "EPSG:6588");
    points.reproject(&SpatialRef::parse("EPSG:6343").unwrap()).unwrap();
    assert!(terrio::warnings::take().is_empty());
}

#[test]
fn reprojection_applies_geoid_models_as_proj() {
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA").map(|d| std::path::PathBuf::from(d).join("grids")) else { return };
    if !dir.join("us_noaa_g2018u0.tif").exists() {
        eprintln!("us_noaa_g2018u0.tif is missing: run scripts/fetch_test_data.sh");
        return;
    }
    terrio::crs::set_grid_search_paths(Some(vec![dir]));
    terrio::crs::set_network_enabled(Some(false));
    let tmp = tempfile::tempdir().unwrap();
    // NAD83(2011) / UTM zone 14N + NAVD88 height through GEOID18, to NAD83(2011) and ITRF2014
    // ellipsoidal heights (PROJ 9.9.0).
    let src = tmp.path().join("utm.laz");
    let src = src.to_str().unwrap();
    pointcloud::write_points(
        &cloud(&[(600000.0, 3320000.0, 100.0), (610000.0, 3330000.0, 50.0)], "EPSG:6343+5703"),
        src,
        &PointWriteOptions { scale: Some(MM), ..Default::default() },
    )
    .unwrap();
    for (dst, expected) in [
        (
            "EPSG:6319",
            [(-97.96312547887827, 30.00687765590811, 74.29647521858018), (-97.85841991962937, 30.096247574602412, 24.166628849074698)],
        ),
        (
            "EPSG:7912",
            [(-97.96313497795812, 30.00688237981196, 73.04581625107676), (-97.85842940319336, 30.09625232482562, 22.91607461962849)],
        ),
    ] {
        let out = tmp.path().join(format!("{}.laz", &dst[5..]));
        let out = out.to_str().unwrap();
        convert(src, out, dst);
        let back = pointcloud::read_points(out, &Default::default()).unwrap();
        for (i, e) in expected.iter().enumerate() {
            let v = |name: &str| back.dim(name).unwrap().values.get_f64(i);
            // Stored to 1e-7 degrees and 1 mm.
            assert!((v("x") - e.0).abs() < 1e-7 && (v("y") - e.1).abs() < 1e-7, "{dst} {i}: {} {}", v("x"), v("y"));
            assert!((v("z") - e.2).abs() < 1e-3, "{dst} {i}: {} vs PROJ {}", v("z"), e.2);
        }
    }
}
