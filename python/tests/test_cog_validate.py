"""terrio.validate_cog, with rio-cogeo's cog_validate as the oracle when it is installed."""

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)


def raster(width, height):
    a = (np.arange(width * height, dtype="float32").reshape(height, width) % 1000).astype("float32")
    return terrio.Raster.from_array(a, TRANSFORM, "EPSG:32610", -9999.0)


@pytest.fixture
def files(tmp_path):
    r = raster(2100, 1500)
    out = {}
    out["cog"] = tmp_path / "cog.tif"
    r.write(str(out["cog"]), driver="COG", compress="DEFLATE")
    out["cog_small"] = tmp_path / "small.tif"
    raster(300, 200).write(str(out["cog_small"]), driver="COG")
    out["strips"] = tmp_path / "strips.tif"
    r.write(str(out["strips"]), driver="GTiff")
    out["tiled_overviews"] = tmp_path / "tiled_ovr.tif"
    r.write(str(out["tiled_overviews"]), driver="GTiff", tile_size=256, overviews=[2, 4])
    out["strip_overviews"] = tmp_path / "strip_ovr.tif"
    r.write(str(out["strip_overviews"]), driver="GTiff", overviews=[2, 4])
    out["not_tiff"] = tmp_path / "x.tif"
    out["not_tiff"].write_bytes(b"definitely not a TIFF")
    return out


def test_verdicts(files):
    assert terrio.validate_cog(str(files["cog"])) == (True, [], [])
    assert terrio.validate_cog(str(files["cog_small"])) == (True, [], [])
    ok, errors, warnings = terrio.validate_cog(str(files["strips"]))
    assert not ok
    assert "The file is greater than 512xH or 512xW, but is not tiled" in errors
    assert warnings == [
        "The file is greater than 512xH or 512xW, it is recommended to include internal overviews"
    ]
    assert not terrio.validate_cog(str(files["tiled_overviews"]))[0]
    ok, errors, _ = terrio.validate_cog(str(files["strip_overviews"]))
    assert "Overview of index 0 is not tiled" in errors
    assert terrio.validate_cog(str(files["not_tiff"])) == (False, ["The file is not a GeoTIFF"], [])
    # Warnings fail strict validation.
    assert terrio.validate_cog(str(files["strips"]), strict=True)[0] is False
    with pytest.raises(terrio.TerrioError):
        terrio.validate_cog(str(files["cog"]) + ".missing")


def test_agrees_with_rio_cogeo(files):
    cogeo = pytest.importorskip("rio_cogeo.cogeo")
    for name, path in files.items():
        if name == "not_tiff":
            continue
        for strict in (False, True):
            expected = cogeo.cog_validate(str(path), strict=strict, quiet=True)
            got = terrio.validate_cog(str(path), strict=strict)
            assert got == (expected[0], list(expected[1]), list(expected[2])), name
