"""Raster blend API, serialization, and validation."""

import numpy as np
import pytest

import terrio


def test_blend_alpha_and_output(tmp_path):
    base = terrio.Raster.from_array(
        np.array([[[30]], [[80]], [[170]], [[255]]], dtype="uint8"),
        (10, 1, 0, 20, 0, -1),
        crs="EPSG:32611",
    )
    overlay = terrio.Raster.from_array(
        np.array([[[250]], [[20]], [[0]], [[0]]], dtype="uint8"), (10, 1, 0, 20, 0, -1)
    )
    out = terrio.blend(base, overlay, str(tmp_path / "out.tif"))
    for band in range(1, 5):
        np.testing.assert_array_equal(out.read(band), base.read(band))
        np.testing.assert_array_equal(
            terrio.open(str(tmp_path / "out.tif")).read(band), base.read(band)
        )
    with pytest.raises(ValueError, match="operator"):
        terrio.blend(base, overlay, operator="bad")
    with pytest.raises(ValueError, match="opacity"):
        terrio.blend(base, overlay, opacity=101)
    assert out.nodata is None


@pytest.mark.parametrize("operator", ["src-over", "hsv-value", "multiply", "color-burn"])
def test_blend_path_to_path_streams_the_in_memory_blend(tmp_path, operator):
    rng = np.random.default_rng(7)
    transform = (500000, 2, 0, 4500000, 0, -2)
    relief = rng.integers(0, 256, (4, 300, 70), dtype="uint8")
    relief[3, :40] = 0
    shade = rng.integers(0, 256, (1, 300, 70), dtype="uint8")
    terrio.Raster.from_array(relief, transform, crs="EPSG:32615").write(
        str(tmp_path / "relief.tif")
    )
    terrio.Raster.from_array(shade, transform, crs="EPSG:32615").write(str(tmp_path / "shade.tif"))
    expected = terrio.blend(
        str(tmp_path / "relief.tif"), str(tmp_path / "shade.tif"), operator=operator, opacity=60
    )
    out = terrio.blend(
        str(tmp_path / "relief.tif"),
        str(tmp_path / "shade.tif"),
        str(tmp_path / "out.tif"),
        operator=operator,
        opacity=60,
    )
    np.testing.assert_array_equal(out.read(), expected.read())
    assert out.transform == expected.transform
    assert out.crs == expected.crs
    assert out.nodata is None
    # An output that is one of the inputs: the inputs are read whole first.
    again = terrio.blend(
        str(tmp_path / "relief.tif"),
        str(tmp_path / "shade.tif"),
        str(tmp_path / "relief.tif"),
        operator=operator,
        opacity=60,
    )
    np.testing.assert_array_equal(again.read(), expected.read())
    np.testing.assert_array_equal(terrio.open(str(tmp_path / "relief.tif")).read(), expected.read())
