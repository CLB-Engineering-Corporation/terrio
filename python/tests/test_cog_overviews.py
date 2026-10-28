"""COG overviews supplied or area-matched, GDAL's PREDICTOR=YES, and strict COG options."""

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)


def river(width=1300, height=1100, nodata=-9999.0):
    """A diagonal band of valid pixels 7 wide: `average` overviews widen it at every level."""
    y, x = np.mgrid[0:height, 0:width]
    a = (1.0 + x % 10).astype("float32")
    a[np.abs(x - y) > 3] = nodata
    return a


def raster(a, nodata=-9999.0):
    return terrio.Raster.from_array(a, TRANSFORM, "EPSG:32610", nodata)


def overviews(path):
    r = terrio.open_raster(str(path), lazy=True)
    return [np.asarray(r.read(overview=i))[0] for i in range(len(r.overviews))]


def test_area_matched_overviews_keep_the_wet_area(tmp_path):
    a = river()
    valid = int((a != -9999).sum())
    avg, am = tmp_path / "avg.tif", tmp_path / "am.tif"
    raster(a).write(str(avg), driver="COG", compress="DEFLATE")
    raster(a).write(str(am), driver="COG", compress="DEFLATE", overview_resampling="area_matched")
    for i, (ov_avg, ov_am) in enumerate(zip(overviews(avg), overviews(am), strict=True)):
        f = 2 ** (i + 1)
        assert int((ov_avg != -9999).sum()) * f * f / valid > 1.2
        assert int((ov_am != -9999).sum()) == round(valid / (f * f))
    # Without nodata there is no wet area to match.
    with pytest.raises(terrio.TerrioError):
        terrio.Raster.from_array(a, TRANSFORM, "EPSG:32610").write(
            str(tmp_path / "x.tif"), driver="COG", overview_resampling="area_matched"
        )


def test_supplied_overviews_round_trip(tmp_path):
    a = river()
    levels = [np.full((550, 650), 3.5, "float32"), np.full((275, 325), -9999, "float32")]
    levels[0][:10] = -9999
    out = tmp_path / "s.tif"
    raster(a).write(
        str(out),
        driver="COG",
        compress="DEFLATE",
        options={"PREDICTOR": "YES", "OVERVIEWS": "FORCE_USE_EXISTING"},
        overviews=[levels[0], raster(levels[1])],
        metadata={"overview_method": "supplied"},
    )
    for got, want in zip(overviews(out), levels, strict=True):
        np.testing.assert_array_equal(got, want)
    np.testing.assert_array_equal(np.asarray(terrio.open_raster(str(out)).read())[0], a)
    # The wrong size is refused, as is resampling supplied overviews.
    with pytest.raises(terrio.TerrioError):
        raster(a).write(
            str(tmp_path / "b.tif"), driver="COG", overviews=[np.zeros((549, 650), "float32")]
        )
    with pytest.raises(terrio.TerrioError):
        raster(a).write(
            str(tmp_path / "c.tif"),
            driver="COG",
            overviews=[levels[0]],
            overview_resampling="average",
        )


@pytest.mark.parametrize("dtype,expected", [("float32", 3), ("int16", 2), ("uint8", 2)])
def test_predictor_yes_follows_gdal_cog(tmp_path, dtype, expected):
    rasterio = pytest.importorskip("rasterio")
    a = (np.arange(64 * 48).reshape(48, 64) % 97).astype(dtype)
    out = tmp_path / "p.tif"
    raster(a, nodata=0).write(
        str(out), driver="COG", compress="DEFLATE", options={"PREDICTOR": "YES"}
    )
    with rasterio.open(out) as src:
        assert src.tags(ns="IMAGE_STRUCTURE")["PREDICTOR"] == str(expected)
        np.testing.assert_array_equal(src.read(1), a)


@pytest.mark.parametrize(
    "options",
    [
        {"TILED": "YES"},
        {"BLOCKXSIZE": "256"},
        {"OVERVIEWS": "FORCE_USE_EXISTING"},
        {"RESAMPLING": "CUBIC"},
        {"COMPRESS": "DEFLATE", "OVERVIEW_COMPRESS": "LZW"},
    ],
)
def test_cog_options_not_acted_on_are_refused(tmp_path, options):
    with pytest.raises(terrio.TerrioError):
        raster(river(64, 48)).write(str(tmp_path / "x.tif"), driver="COG", options=options)


def test_gdal_reads_terrio_cogs_as_its_own(tmp_path):
    rasterio = pytest.importorskip("rasterio")
    cogeo = pytest.importorskip("rio_cogeo.cogeo")
    a = river()
    out = tmp_path / "l.tif"
    raster(a).write(
        str(out),
        driver="COG",
        compress="LERC",
        options={"MAX_Z_ERROR": "0.01", "OVERVIEW_RESAMPLING": "area_matched"},
        metadata={"compression_lossy": "true"},
    )
    assert cogeo.cog_validate(str(out), quiet=True)[0]
    with rasterio.open(out) as src:
        ims = src.tags(ns="IMAGE_STRUCTURE")
        assert ims["LAYOUT"] == "COG"
        assert ims["MAX_Z_ERROR"] == "0.01"
        assert ims["OVERVIEW_RESAMPLING"] == "AREA_MATCHED"
        assert src.tags()["compression_lossy"] == "true"
        assert np.nanmax(np.abs(src.read(1) - a)) <= 0.01 + 1e-6


def test_cog_overview_factors_are_written(tmp_path):
    a = river()
    cog, gtiff = tmp_path / "f.tif", tmp_path / "g.tif"
    raster(a).write(str(cog), driver="COG", overviews=[2, 4, 8], overview_resampling="average")
    raster(a).write(str(gtiff), driver="GTiff", overviews=[2, 4, 8], overview_resampling="average")
    got, want = overviews(cog), overviews(gtiff)
    assert [o.shape for o in got] == [(550, 650), (275, 325), (138, 163)]
    for g, w in zip(got, want, strict=True):
        np.testing.assert_array_equal(g, w)
    assert terrio.validate_cog(str(cog))[0]
    for options in ({"OVERVIEWS": "NONE"}, {"OVERVIEWS": "IGNORE_EXISTING"}):
        with pytest.raises(terrio.TerrioError):
            raster(a).write(str(tmp_path / "x.tif"), driver="COG", overviews=[2], options=options)


def test_cog_overview_factors_match_gdal_reuse(tmp_path):
    """GDAL: gdaladdo the factors on the source, then the COG driver reuses them (OVERVIEWS=AUTO)."""
    rasterio = pytest.importorskip("rasterio")
    from rasterio.enums import Resampling
    from rasterio.shutil import copy

    a = river()
    src = tmp_path / "src.tif"
    raster(a).write(str(src))
    with rasterio.open(src, "r+") as ds:
        ds.build_overviews([2, 4, 8], Resampling.nearest)
    gdal = tmp_path / "gdal.tif"
    copy(src, gdal, driver="COG")
    ours = tmp_path / "ours.tif"
    raster(a).write(str(ours), driver="COG", overviews=[2, 4, 8], overview_resampling="nearest")
    with rasterio.open(gdal) as g, rasterio.open(ours) as o:
        assert g.overviews(1) == o.overviews(1) == [2, 4, 8]
        for f in (2, 4, 8):
            shape = (1, -(-a.shape[0] // f), -(-a.shape[1] // f))
            np.testing.assert_array_equal(g.read(out_shape=shape), o.read(out_shape=shape))
