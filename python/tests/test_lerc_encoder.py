"""LERC tiles written with the default ``LERC_ENCODER=GDAL`` are GDAL's, blob for blob.

GDAL (rasterio) is the oracle: the same raster is written by both, and every tile's Lerc2 blob
(inflated for ``LERC_DEFLATE``) is compared byte for byte. ``LERC_ENCODER=COMPACT`` writes
other, no larger blobs within the same ``MAX_Z_ERROR``.
"""

import zlib

import numpy as np
import pytest

import terrio

rasterio = pytest.importorskip("rasterio")
rasterio_shutil = pytest.importorskip("rasterio.shutil")
MemoryFile = pytest.importorskip("rasterio.io").MemoryFile

W, H, TILE = 300, 200, 128
GT = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)


def _array(dtype, nodata):
    y, x = np.mgrid[0:H, 0:W].astype("float64")
    a = 100 + 0.013 * x + 0.007 * y + 2 * np.sin(x / 17) * np.cos(y / 23)
    if np.dtype(dtype).kind == "f":
        a = np.round(a, 3)  # decimal-grid values exercise the error-bound raise
    else:
        a = a * 37
    a = a.astype(dtype)
    if nodata is not None:
        a[40:60, 50:90] = nodata
        a[H - 10 :, :] = nodata  # nodata along the bottom edge
    return a[None]


def _tiles(path):
    out = []
    with rasterio.open(path) as d, open(path, "rb") as f:
        for ty in range(-(-d.height // TILE)):
            for tx in range(-(-d.width // TILE)):
                off = d.get_tag_item(f"BLOCK_OFFSET_{tx}_{ty}", "TIFF", bidx=1)
                size = d.get_tag_item(f"BLOCK_SIZE_{tx}_{ty}", "TIFF", bidx=1)
                f.seek(int(off))
                out.append(f.read(int(size)))
    return out


def _write(tmp_path, a, nodata, driver, compress, mze, encoder=None):
    gdal_path, terrio_path = tmp_path / "gdal.tif", tmp_path / f"terrio-{encoder}.tif"
    profile = dict(
        driver="GTiff",
        width=W,
        height=H,
        count=1,
        dtype=a.dtype,
        nodata=nodata,
        crs="EPSG:32615",
        transform=rasterio.Affine.from_gdal(*GT),
    )
    with MemoryFile() as mem, mem.open(**profile) as src:
        src.write(a)
        extra = {"max_z_error": mze}
        if driver == "COG":
            extra.update(blocksize=TILE, overviews="NONE")
        else:
            extra.update(tiled=True, blockxsize=TILE, blockysize=TILE)
        rasterio_shutil.copy(src, gdal_path, driver=driver, compress=compress, **extra)
    options = {"MAX_Z_ERROR": str(mze)}
    if driver == "COG":
        options["OVERVIEWS"] = "NONE"
    else:
        options["TILED"] = "YES"
    if encoder is not None:
        options["LERC_ENCODER"] = encoder
    terrio.Raster.from_array(a, GT, crs="EPSG:32615", nodata=nodata).write(
        str(terrio_path), driver=driver, compress=compress, tile_size=TILE, options=options
    )
    return gdal_path, terrio_path


def _blob(tile, compress):
    return zlib.decompress(tile) if compress == "LERC_DEFLATE" else tile


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("compress", ["LERC", "LERC_DEFLATE"])
@pytest.mark.parametrize(
    ("dtype", "nodata", "mze"),
    [
        ("float32", float("nan"), 0.01),
        ("float32", float("nan"), 0.0),
        ("float32", -9999.0, 0.001),
        ("float32", None, 0.01),
        ("float64", float("nan"), 0.01),
        ("int16", None, 0.0),
        ("uint16", 65535, 0.0),
    ],
)
def test_default_lerc_blobs_are_gdal_s(tmp_path, driver, compress, dtype, nodata, mze):
    a = _array(dtype, nodata)
    gdal_path, terrio_path = _write(tmp_path, a, nodata, driver, compress, mze)
    gdal, ours = _tiles(gdal_path), _tiles(terrio_path)
    assert len(gdal) == len(ours)
    for i, (g, t) in enumerate(zip(gdal, ours, strict=True)):
        assert _blob(g, compress) == _blob(t, compress), f"tile {i}"


def test_compact_lerc_blobs_are_no_larger_within_the_error(tmp_path):
    a = _array("float32", float("nan"))
    _, gdal_like = _write(tmp_path, a, float("nan"), "GTiff", "LERC", 0.01, "GDAL")
    _, compact = _write(tmp_path, a, float("nan"), "GTiff", "LERC", 0.01, "COMPACT")
    assert sum(map(len, _tiles(compact))) <= sum(map(len, _tiles(gdal_like)))
    with rasterio.open(compact) as c:
        back = c.read(1)
    valid = ~np.isnan(a[0])
    assert np.array_equal(np.isnan(back), ~valid)
    # Within MAX_Z_ERROR, up to the Float32 rounding of the stored values (an ulp near 100).
    err = np.abs(back[valid].astype("float64") - a[0][valid].astype("float64")).max()
    assert err <= 0.01 + 1e-5
    with pytest.raises(terrio.TerrioError, match="LERC_ENCODER"):
        terrio.Raster.from_array(a, GT, nodata=float("nan")).write(
            str(tmp_path / "x.tif"), compress="LERC", options={"LERC_ENCODER": "FAST"}
        )
