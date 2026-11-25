"""`terrio.GeoTiffWriter.write_tile`: a tiled GeoTIFF written tile by tile, in any order.

The file reads back, through GDAL, as the tiles given; tiles never given read as nodata, written
as nodata or left out with `SPARSE_OK=YES`, as GDAL writes or leaves out the blocks it was never
given. A file is written in tiles or in rows, and tiles are refused by strips.
"""

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
TILED = {"TILED": "YES", "BLOCKXSIZE": "16", "BLOCKYSIZE": "16"}


def tiles_of(writer, data, skip=()):
    """The tiles of `data` (bands, rows, cols) in a fixed shuffled order, but those in `skip`."""
    _, _, across, down = writer.tile_grid
    tiles = [(c, r) for r in range(down) for c in range(across) if (c, r) not in skip]
    np.random.default_rng(5).shuffle(tiles)
    for c, r in tiles:
        yield c, r, data[:, r * 16 : (r + 1) * 16, c * 16 : (c + 1) * 16]


@pytest.mark.parametrize(
    ("dtype", "options"),
    [
        ("float32", {"PREDICTOR": "3"}),
        ("int16", {"PREDICTOR": "2", "INTERLEAVE": "BAND"}),
        ("uint8", {}),
        ("float64", {"PREDICTOR": "2"}),
    ],
)
@pytest.mark.parametrize("readonly", [False, True])
def test_tiles_in_any_order_read_back_as_given(tmp_path, gdal, dtype, options, readonly):
    """Shuffled tiles, edge tiles cut by the raster's edges, read back by GDAL as given."""
    rng = np.random.default_rng(3)
    data = (rng.random((2, 35, 65)) * 200 - 100).astype(dtype)
    path = tmp_path / "tiles.tif"
    w = terrio.GeoTiffWriter(
        str(path),
        65,
        35,
        2,
        dtype,
        TRANSFORM,
        "EPSG:32610",
        compress="ZSTD",
        options={**TILED, **options},
    )
    assert w.tile_grid == (16, 16, 5, 3)
    for c, r, tile in tiles_of(w, data):
        tile = np.ascontiguousarray(tile)
        if readonly:
            tile.setflags(write=False)
        w.write_tile(c, r, tile)
    assert (w.tiles_written, w.rows_written) == (15, 0)
    w.close()
    ds = gdal.Open(str(path))
    np.testing.assert_array_equal(ds.ReadAsArray(), data)
    assert ds.GetRasterBand(1).GetBlockSize() == [16, 16]


@pytest.mark.parametrize("sparse", [False, True])
def test_tiles_never_given_read_as_nodata(tmp_path, gdal, sparse):
    """Tiles never given are nodata: written so, or left out with `SPARSE_OK=YES`."""
    data = np.arange(35 * 65, dtype="float32").reshape(1, 35, 65)
    path = tmp_path / "partial.tif"
    options = {**TILED, "SPARSE_OK": "YES" if sparse else "NO"}
    w = terrio.GeoTiffWriter(
        str(path), 65, 35, 1, "float32", TRANSFORM, None, -9999.0, options=options
    )
    skip = {(4, 2), (1, 0)}
    for c, r, tile in tiles_of(w, data, skip):
        w.write_tile(c, r, tile[0])
    w.close()
    expected = data[0].copy()
    expected[32:35, 64:65] = -9999.0
    expected[0:16, 16:32] = -9999.0
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    np.testing.assert_array_equal(band.ReadAsArray(), expected)
    offsets = [band.GetMetadataItem(f"BLOCK_OFFSET_{c}_{r}", "TIFF") for c, r in sorted(skip)]
    assert all(o in (None, "", "0") for o in offsets) == sparse


def test_tiles_out_of_place_are_refused(tmp_path):
    """Strips have no tiles; a tile is written once, of its shape, and not after rows."""
    strips = terrio.GeoTiffWriter(str(tmp_path / "strips.tif"), 65, 35, 1, "uint8", TRANSFORM)
    assert strips.tile_grid is None
    with pytest.raises(terrio.UnsupportedError):
        strips.write_tile(0, 0, np.zeros((16, 16), "uint8"))
    w = terrio.GeoTiffWriter(
        str(tmp_path / "tiles.tif"), 65, 35, 1, "uint8", TRANSFORM, options=TILED
    )
    with pytest.raises(terrio.InvalidArgumentError):
        w.write_tile(4, 0, np.zeros((16, 16), "uint8"))  # an edge tile of 16 x 1
    with pytest.raises(terrio.InvalidArgumentError):
        w.write_tile(0, 0, np.zeros((16, 16), "int16"))
    with pytest.raises(terrio.InvalidArgumentError):
        w.write_tile(5, 0, np.zeros((16, 16), "uint8"))
    w.write_tile(4, 0, np.zeros((16, 1), "uint8"))
    with pytest.raises(terrio.InvalidArgumentError):
        w.write_tile(4, 0, np.zeros((16, 1), "uint8"))
    with pytest.raises(terrio.InvalidArgumentError):
        w.write(np.zeros((1, 1, 65), "uint8"))
    w.close()
    rows = terrio.GeoTiffWriter(
        str(tmp_path / "rows.tif"), 65, 35, 1, "uint8", TRANSFORM, options=TILED
    )
    rows.write(np.zeros((1, 1, 65), "uint8"))
    with pytest.raises(terrio.InvalidArgumentError):
        rows.write_tile(0, 0, np.zeros((16, 16), "uint8"))
