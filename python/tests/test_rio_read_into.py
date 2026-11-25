# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Caller-buffer TIFF samples compared with GDAL 3.12 via rasterio."""

import numpy as np
import pytest

import terrio.rio as rio

rasterio = pytest.importorskip("rasterio")


@pytest.mark.parametrize("tiled", [False, True])
@pytest.mark.parametrize("interleave", ["pixel", "band"])
@pytest.mark.parametrize("dtype", ["uint8", "int16", "float32"])
@pytest.mark.parametrize("compression", ["none", "deflate"])
def test_destination_matches_gdal(tmp_path, monkeypatch, tiled, interleave, dtype, compression):
    path = tmp_path / "source.tif"
    values = (np.arange(3 * 35 * 39).reshape(3, 35, 39) % 237).astype(dtype)
    nodata = 255 if dtype == "uint8" else -999
    values[:, 24, 33] = nodata
    if dtype == "float32":
        values[:, 25, 34] = np.nan
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=39,
        height=35,
        count=3,
        dtype=dtype,
        nodata=nodata,
        tiled=tiled,
        blockxsize=16 if tiled else 39,
        blockysize=16,
        compress=compression,
        interleave=interleave,
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.scales = (2, 3, 4)
        dst.offsets = (10, -20, 30)
        dst.write(values)
    window = rasterio.windows.Window(7, 13, 32, 22)
    with rio.open(path) as actual, rasterio.open(path) as reference:

        def owned_read(*args, **kwargs):
            pytest.fail("a contiguous native output should bypass the owned read")

        for b in (1, 2, 3):
            out = np.full((22, 32), 42, dtype=dtype)
            # Require the native destination path, independently of the adapter's fallback.
            assert actual._raster._read_geotiff_into(b, (7, 13, 32, 22), out)
            np.testing.assert_array_equal(out, reference.read(b, window=window))
            out.fill(42)
            with monkeypatch.context() as patch:
                patch.setattr(actual, "_io", owned_read)
                got = actual.read(b, window=((13, 35), (7, 39)), out=out)
            assert np.shares_memory(got, out)
            np.testing.assert_array_equal(got, actual.read(b, window=((13, 35), (7, 39))))
            np.testing.assert_array_equal(got, reference.read(b, window=window))
        strided = np.empty((22, 64), dtype=dtype)[:, ::2]
        got = actual.read(1, window=((13, 35), (7, 39)), out=strided)
        assert np.shares_memory(got, strided)
        np.testing.assert_array_equal(got, reference.read(1, window=window))
        assert actual.nodata == reference.nodata
        assert actual.scales == reference.scales
        assert actual.offsets == reference.offsets


def test_sparse_destination_matches_gdal(tmp_path):
    path = tmp_path / "sparse.tif"
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=35,
        height=33,
        count=1,
        dtype="int16",
        nodata=-32768,
        tiled=True,
        blockxsize=16,
        blockysize=16,
        compress="deflate",
        SPARSE_OK=True,
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(
            np.full((16, 16), 12, dtype="int16"), 1, window=rasterio.windows.Window(16, 0, 16, 16)
        )
    out = np.full((33, 35), 37, dtype="int16")
    with rio.open(path) as actual, rasterio.open(path) as reference:
        assert actual._raster._read_geotiff_into(1, (0, 0, 35, 33), out)
        assert out[0, 0] == -32768
        assert out[0, 16] == 12
        np.testing.assert_array_equal(out, reference.read(1))
        np.testing.assert_array_equal(actual.read(1, out=out), actual.read(1))


def test_complex_destination_uses_existing_conversion(tmp_path):
    path = tmp_path / "complex.tif"
    values = np.array([[1 + 2j, 3 - 4j]], dtype="complex64")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=2,
        height=1,
        count=1,
        dtype="complex64",
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(values, 1)
    with rio.open(path) as actual, rasterio.open(path) as reference:
        out = np.empty_like(values)
        got = actual.read(1, out=out)
        assert np.shares_memory(got, out)
        np.testing.assert_array_equal(got, reference.read(1))


def test_lzw_destination_matches_gdal_on_terrain(tmp_path, data_dir):
    gdal = pytest.importorskip("osgeo.gdal")
    source = data_dir / "dem.tif"
    if not source.exists():
        pytest.skip("needs dem.tif")
    path = tmp_path / "terrain.tif"
    dataset = gdal.Translate(
        str(path),
        str(source),
        srcWin=[7, 11, 513, 401],
        outputType=gdal.GDT_Float32,
        creationOptions=["TILED=YES", "COMPRESS=LZW", "PREDICTOR=3"],
    )
    dataset.Close()
    reference = gdal.Open(str(path))
    expected = reference.ReadAsArray(3, 5, 510, 396)
    out = np.empty((396, 510), dtype="float32")
    with rio.open(path) as actual:
        # Require the positioned decoder, including both edge tiles and the predictor.
        assert actual._raster._read_geotiff_into(1, (3, 5, 510, 396), out)
        np.testing.assert_array_equal(out, expected)
        np.testing.assert_equal(actual.nodata, reference.GetRasterBand(1).GetNoDataValue())
        assert actual.transform.to_gdal() == reference.GetGeoTransform()
