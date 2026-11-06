//! HDF5 userblock text cannot change the driver used for in-memory allow-list checks.
#![cfg(feature = "netcdf-hdf5")]

/// A separate test process lets the allow-list narrow from both drivers to netCDF only.
#[test]
fn cdf_userblock_is_hdf5_for_owned_and_borrowed_inputs() {
    use terrio::formats::{Kind, set_allowed_drivers};
    let mut bytes = include_bytes!("data/multidim-userblock.h5").to_vec();
    bytes[..4].copy_from_slice(b"CDF\x01");
    set_allowed_drivers(Some(&["netCDF", "HDF5"]), Some(Kind::Raster)).unwrap();
    for ds in [terrio::multidim::open_bytes(&bytes).unwrap(), terrio::multidim::open_bytes_owned(bytes.clone()).unwrap()] {
        assert_eq!(ds.driver, "HDF5");
        let view = ds.array("sample").unwrap().view();
        drop(ds);
        assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
    }
    set_allowed_drivers(Some(&["netCDF"]), Some(Kind::Raster)).unwrap();
    for result in [terrio::multidim::open_bytes(&bytes), terrio::multidim::open_bytes_owned(bytes)] {
        let message = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("HDF5 must be refused"),
        };
        assert!(message.contains("not allowed") || message.contains("no allowed driver"), "{message}");
    }
}
