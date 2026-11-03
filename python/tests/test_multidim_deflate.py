"""Packed CF data survive the native compressed snapshot writer."""

import numpy as np
import pytest

import terrio


def test_compressed_snapshot(tmp_path):
    netcdf = pytest.importorskip("netCDF4")
    source = tmp_path / "source.nc"
    target = tmp_path / "target.nc"
    with netcdf.Dataset(source, "w") as nc:
        for name, size in [("flow", 2), ("y", 2), ("x", 3)]:
            nc.createDimension(name, size)
            var = nc.createVariable(name, "f8", (name,))
            var[:] = np.arange(size)
        depth = nc.createVariable(
            "depth", "u2", ("flow", "y", "x"), chunksizes=(1, 2, 3), fill_value=65535
        )
        depth.set_auto_maskandscale(False)
        depth.scale_factor = np.float32(0.01)
        depth.add_offset = np.float32(0)
        depth[:] = np.arange(12, dtype="uint16").reshape(2, 2, 3)
    terrio.open_multidim(source).write(target, deflate=4)
    with netcdf.Dataset(target) as nc:
        depth = nc.variables["depth"]
        depth.set_auto_maskandscale(False)
        np.testing.assert_array_equal(depth[:].ravel(), np.arange(12))
        assert depth.filters()["zlib"]
        assert depth.filters()["complevel"] == 4
        assert depth.chunking() == [1, 2, 3]
        assert depth.scale_factor.dtype == np.dtype("float32")
        assert depth._FillValue.dtype == np.dtype("uint16")
    with pytest.raises(ValueError, match="deflate"):
        terrio.open_multidim(source).to_bytes(deflate=10)
