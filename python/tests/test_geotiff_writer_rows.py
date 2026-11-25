"""`terrio.GeoTiffWriter.write`: rows read where they are, or copied, make the same file.

A read-only C-contiguous array of the writer's dtype is compressed where it is; any other array
is copied first. Either way the file is byte for byte the same, however the rows are split into
calls, and GDAL reads back the rows given.
"""

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)


def write(path, data, splits, options, readonly):
    """Writes `data` (bands, rows, cols) in calls of `splits` rows, then the rows left."""
    bands, rows, cols = data.shape
    w = terrio.GeoTiffWriter(
        str(path),
        cols,
        rows,
        bands,
        data.dtype.name,
        TRANSFORM,
        compress="DEFLATE",
        options=options,
    )
    y = 0
    for n in [*splits, rows - sum(splits)]:
        part = np.ascontiguousarray(data[:, y : y + n])
        if readonly:
            part.setflags(write=False)
        w.write(part)
        y += n
    assert w.rows_written == rows
    w.close()
    return path.read_bytes()


@pytest.mark.parametrize(
    ("dtype", "options"),
    [
        ("float32", {"TILED": "YES", "BLOCKXSIZE": "16", "BLOCKYSIZE": "16", "PREDICTOR": "3"}),
        ("int16", {"BLOCKYSIZE": "16", "PREDICTOR": "2"}),
        ("uint8", {"TILED": "YES", "BLOCKXSIZE": "16", "BLOCKYSIZE": "16", "INTERLEAVE": "BAND"}),
    ],
)
def test_read_only_rows_write_the_file_copied_rows_write(tmp_path, gdal, dtype, options):
    """Read-only rows, compressed where they are, and writable rows, copied, give one file."""
    rng = np.random.default_rng(7)
    data = (rng.random((2, 35, 65)) * 200 - 100).astype(dtype)
    copied = write(tmp_path / "copied.tif", data, [], options, readonly=False)
    for splits in ([], [32], [5, 11, 16], [1] * 34):
        borrowed = write(tmp_path / "borrowed.tif", data, splits, options, readonly=True)
        assert borrowed == copied, splits
    ds = gdal.Open(str(tmp_path / "borrowed.tif"))
    np.testing.assert_array_equal(ds.ReadAsArray(), data)


def test_read_only_rows_of_another_dtype_are_refused(tmp_path):
    """A read-only array is not converted: rows of another dtype are refused, none taken."""
    w = terrio.GeoTiffWriter(str(tmp_path / "typed.tif"), 65, 35, 1, "float32", TRANSFORM)
    rows = np.zeros((1, 35, 65), dtype="float64")
    rows.setflags(write=False)
    with pytest.raises(terrio.InvalidArgumentError):
        w.write(rows)
    assert w.rows_written == 0
    w.write(rows.astype("float32"))
    w.close()


def test_strided_read_only_rows_are_copied(tmp_path):
    """A read-only view that is not C-contiguous is copied, and written as it reads."""
    data = np.arange(2 * 35 * 130, dtype="float32").reshape(2, 35, 130)
    view = data[:, :, ::2]
    view.setflags(write=False)
    w = terrio.GeoTiffWriter(str(tmp_path / "strided.tif"), 65, 35, 2, "float32", TRANSFORM)
    w.write(view)
    w.close()
    back = terrio.open(str(tmp_path / "strided.tif")).read()
    np.testing.assert_array_equal(np.asarray(back), view)
