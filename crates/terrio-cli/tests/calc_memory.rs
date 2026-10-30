//! `terrio calc` reads its inputs by window and writes a strip of rows at a time (`--max-memory`).

use std::path::Path;
use std::process::{Command, Output};
use terrio::ops::calc::{CalcExtent, CalcInput, CalcOptions, calc};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn raster(x_min: f64, y_min: f64, cols: usize, rows: usize, k: usize) -> terrio::RasterDataset {
    let cfg = terrio::RasterConfig {
        cols,
        rows,
        x_min,
        y_min,
        cell_size: 30.0,
        nodata: -9999.0,
        data_type: terrio::DataType::F32,
        ..Default::default()
    };
    let v = (0..cols * rows).map(|i| if i % 97 == k { -9999.0 } else { ((i * k) % 251) as f64 + 0.25 }).collect();
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, v).unwrap(), true);
    r.raster.crs.epsg = Some(32611);
    r
}

fn bits(ds: &terrio::RasterDataset) -> Vec<u64> {
    ds.raster.data.to_f64_vec().iter().map(|v| v.to_bits()).collect()
}

#[test]
fn calc_is_streamed_with_the_values_in_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // B is off A's grid by 0.4 and 0.7 of a pixel, and smaller.
    let (a, b) = (raster(500000.0, 4000000.0, 300, 200, 3), raster(500000.0 + 30.0 * 40.4, 4000000.0 + 30.0 * 20.7, 150, 90, 7));
    for (r, name) in [(&a, "a.tif"), (&b, "b.tif")] {
        terrio::write_raster_dataset(r, d.join(name).to_str().unwrap(), &Default::default()).unwrap();
    }
    let (a, b) =
        (terrio::open_raster(d.join("a.tif").to_str().unwrap()).unwrap(), terrio::open_raster(d.join("b.tif").to_str().unwrap()).unwrap());
    for (extent, args) in [
        (CalcExtent::Ignore, &["-A", "a.tif", "--calc", "A * 0.3048"][..]),
        (CalcExtent::Union, &["-A", "a.tif", "-B", "b.tif", "--calc", "A + B", "--extent", "union"][..]),
    ] {
        let inputs: Vec<CalcInput> = [("A", &a), ("B", &b)]
            .into_iter()
            .take(if extent == CalcExtent::Ignore { 1 } else { 2 })
            .map(|(name, ds)| CalcInput { name: name.into(), ds, band: 1 })
            .collect();
        let o =
            CalcOptions { calc: vec![args[args.iter().position(|x| *x == "--calc").unwrap() + 1].into()], extent, ..Default::default() };
        let whole = calc(&inputs, &o).unwrap();
        for budget in [None, Some("0.01")] {
            let mut all: Vec<&str> = vec!["calc"];
            all.extend_from_slice(args);
            all.push("out.tif");
            if let Some(m) = budget {
                all.extend_from_slice(&["--max-memory", m]);
            }
            let r = run(&all, d);
            assert!(r.status.success(), "{args:?} {budget:?}: {}", String::from_utf8_lossy(&r.stderr));
            let out = terrio::open_raster(d.join("out.tif").to_str().unwrap()).unwrap();
            assert_eq!(bits(&out), bits(&whole), "{args:?} {budget:?}");
            assert_eq!(
                (out.geo_transform(), out.nodata(), out.raster.data_type),
                (whole.geo_transform(), whole.nodata(), whole.raster.data_type)
            );
        }
    }
    // An output that is an input is computed whole first.
    terrio::write_raster_dataset(&a, d.join("same.tif").to_str().unwrap(), &Default::default()).unwrap();
    let r = run(&["calc", "-A", "same.tif", "--calc", "A + 1", "same.tif"], d);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let o = CalcOptions { calc: vec!["A + 1".into()], ..Default::default() };
    let plus = calc(&[CalcInput { name: "A".into(), ds: &a, band: 1 }], &o).unwrap();
    assert_eq!(bits(&terrio::open_raster(d.join("same.tif").to_str().unwrap()).unwrap()), bits(&plus));
    for bad in ["0", "-1", "nan"] {
        assert!(!run(&["calc", "-A", "a.tif", "--calc", "A", "bad.tif", "--max-memory", bad], d).status.success(), "{bad}");
    }
}
