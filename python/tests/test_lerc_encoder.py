"""LERC tiles written with the default ``LERC_ENCODER=GDAL`` are GDAL's, blob for blob.

GDAL (rasterio) is the oracle: the same raster is written by both, and every stored chunk's
Lerc2 blob, of every band and overview level, is compared byte for byte. The blob is the chunk
itself for ``LERC``; for ``LERC_DEFLATE`` and ``LERC_ZSTD`` it is the chunk inflated or
decompressed, since the outer stream is zlib's or libzstd's, not LERC's. The cases cover every
data type LERC stores (Int8 to UInt32, Float32, Float64), lossless and lossy, one band and three
bands pixel- and band-interleaved, NaN, a nodata value or a per-dataset mask, as GTiff and COG,
in 128 x 128 tiles of a 300 x 200 raster, so the right and bottom tiles are partial.
``LERC_ENCODER=COMPACT`` writes other, no larger blobs within the same ``MAX_Z_ERROR``.

Known differences:

- Lossless Int8 and Byte data (``MAX_Z_ERROR`` below 1), in the tiles libLerc codes with Huffman
  codes (where they are smaller than its blocks). lerc-writer 0.5.2 writes a version 2 code table
  and its own codes there, where libLerc writes version 4, so those blobs differ; the other tiles'
  blobs and every pixel are GDAL's (``test_lossless_8bit_lerc_reads_as_gdal_s``), and
  ``test_lossless_8bit_lerc_blobs_are_gdal_s`` fails until lerc-writer matches libLerc.
- Lossy Float64 data on a decimal grid with a finite nodata value far from it (-9999): in a block
  holding both, libLerc's reconstruction misses ``MAX_Z_ERROR`` by about 1e-12 through ``f64``
  rounding, and lerc-writer 0.5.2's GDAL profile, which checks for that, stores the block raw
  instead. Those tiles differ (``FLOAT64_SENTINEL_CASES``, strict expected failures); the others
  are GDAL's and the pixels are within ``MAX_Z_ERROR``
  (``test_lossy_float64_sentinel_reads_within_the_error``).
- The module asserts GDAL 3.12's blobs. GDAL 3.13 also masks the padding of partial edge tiles of
  pixel-interleaved floating-point bands, which terrio (as GDAL 3.12) does not, so those rows are
  strict expected failures with GDAL 3.13 and later (``GDAL_313_PADDING``).

Reference: GDAL 3.12.2 (rasterio 1.5.2); the pixel-interleaved floating-point rows also checked to
differ, as expected, from GDAL 3.13.3.
"""

import struct
import warnings
import zlib

import numpy as np
import pytest

import terrio

rasterio = pytest.importorskip("rasterio")
rasterio_shutil = pytest.importorskip("rasterio.shutil")
MemoryFile = pytest.importorskip("rasterio.io").MemoryFile

# The reference's major.minor version, read as conftest.py's `_reference_gdal_version` reads it.
GDAL_VERSION = tuple(int(part) for part in rasterio.__gdal_version__.split(".")[:2])
# GDAL 3.13 masks the padding of pixel-interleaved floating-point edge tiles; terrio writes GDAL
# 3.12's blobs there (see the module's docstring).
GDAL_313_PADDING = pytest.mark.xfail(
    GDAL_VERSION >= (3, 13),
    strict=True,
    raises=AssertionError,
    reason="GDAL 3.13 masks the padding of pixel-interleaved floating-point edge tiles, which "
    "GDAL 3.12 and terrio do not",
)
W, H, TILE = 300, 200, 128
GT = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
NAN = float("nan")

# The value range of each integer type's data: (centre, half-span). UInt32 lies above Int32's
# range and Int32 well below zero, so the samples' full widths are used.
INTEGER_RANGE = {
    "int8": (0, 100),
    "uint8": (128, 120),
    "int16": (0, 30000),
    "uint16": (32768, 30000),
    "int32": (-100_000_000, 2_000_000),
    "uint32": (3_000_000_000, 2_000_000),
}


def _array(dtype, bands=1, nodata=None, holes="same"):
    """A smooth surface with noise, per band: rounded to 0.001 for floats, scaled for integers.

    Args:
        dtype: The numpy data type.
        bands: The number of bands.
        nodata: A value written in holes (the same in every band, plus one per band with
            ``holes="differ"``), and along the bottom edge; None for none.
        holes: ``"same"`` or ``"differ"``.

    Returns:
        A ``(bands, H, W)`` array.
    """
    y, x = np.mgrid[0:H, 0:W].astype("float64")
    rng = np.random.default_rng(20261125)
    out = []
    for b in range(bands):
        s = 0.013 * x + 0.007 * y + 2 * np.sin(x / 17 + b) * np.cos(y / 23)  # about -2 to 6
        if np.dtype(dtype).kind == "f":
            # Decimal-grid values exercise the error-bound raise.
            a = np.round(100 + 10 * b + s + rng.normal(0, 0.01, s.shape), 3)
        else:
            info = np.iinfo(dtype)
            centre, half = INTEGER_RANGE[dtype]
            a = centre + (s - 2) / 4 * half + rng.integers(-3, 4, s.shape)
            a = np.clip(np.round(a), info.min + 1, info.max - 1)  # nodata may be either extreme
        out.append(a.astype(dtype))
    a = np.stack(out)
    if nodata is not None:
        a[:, 40:60, 50:90] = nodata
        a[:, H - 10 :, :] = nodata  # nodata along the bottom edge
        if holes == "differ":
            for b in range(bands):
                a[b, 100:120, 20 + 60 * b : 60 + 60 * b] = nodata
    return a


def _mask():
    """A per-dataset mask: 0 in a block and along the right edge, 255 elsewhere."""
    m = np.full((H, W), 255, np.uint8)
    m[70:90, 150:260] = 0
    m[:, W - 5 :] = 0
    return m


# (data type, bands, interleave, nodata, holes, mask, MAX_Z_ERROR)
CASES = []
for _dt in ["int16", "uint16", "int32", "uint32"]:
    CASES += [
        (_dt, 1, "band", None, "same", False, 0.0),
        (_dt, 1, "band", None, "same", False, 1.0),
    ]
for _dt in ["int8", "uint8"]:  # lossless in HUFFMAN_CASES
    CASES += [
        (_dt, 1, "band", None, "same", False, 1.0),
        (_dt, 1, "band", None, "same", False, 2.5),
    ]
for _dt in ["float32", "float64"]:
    CASES += [
        (_dt, 1, "band", None, "same", False, 0.0),
        (_dt, 1, "band", None, "same", False, 0.01),
    ]
CASES += [
    # One band with nodata.
    ("float32", 1, "band", NAN, "same", False, 0.01),
    ("float32", 1, "band", NAN, "same", False, 0.0),
    ("float32", 1, "band", -9999.0, "same", False, 0.001),
    ("float64", 1, "band", NAN, "same", False, 0.01),
    ("int8", 1, "band", -128, "same", False, 1.0),
    ("uint8", 1, "band", 255, "same", False, 1.0),
    ("int16", 1, "band", -32768, "same", False, 1.0),
    ("uint16", 1, "band", 65535, "same", False, 0.0),
    ("int32", 1, "band", -2147483648, "same", False, 0.0),
    ("uint32", 1, "band", 0, "same", False, 1.0),
    # Three bands, pixel- and band-interleaved.
    ("uint8", 3, "pixel", None, "same", False, 1.0),
    ("uint8", 3, "band", None, "same", False, 1.0),
    ("int16", 3, "pixel", -32768, "differ", False, 1.0),
    ("int16", 3, "band", -32768, "differ", False, 1.0),
    ("uint16", 3, "pixel", None, "same", False, 0.0),
    ("int32", 3, "pixel", None, "same", False, 0.0),
    ("uint32", 3, "band", None, "same", False, 1.0),
    ("float32", 3, "pixel", None, "same", False, 0.01),
    ("float32", 3, "pixel", NAN, "same", False, 0.01),
    ("float32", 3, "band", NAN, "differ", False, 0.01),
    ("float64", 3, "pixel", -9999.0, "differ", False, 0.0),
    ("float64", 3, "pixel", NAN, "same", False, 0.0),
    # A per-dataset mask (GDAL's internal mask).
    ("uint8", 1, "band", None, "same", True, 1.0),
    ("uint16", 3, "pixel", None, "same", True, 0.0),
    ("uint16", 3, "band", None, "same", True, 0.0),
    ("float32", 1, "band", None, "same", True, 0.01),
    ("float32", 3, "pixel", None, "same", True, 0.01),
    ("float32", 1, "band", NAN, "same", True, 0.01),
]
# Lossy Float64 on a decimal grid with a finite nodata value: lerc-writer stores raw the blocks
# where libLerc's reconstruction exceeds MAX_Z_ERROR by f64 rounding (see the docstring).
FLOAT64_SENTINEL_CASES = [
    ("float64", 1, "band", -9999.0, "same", False, 0.01),
    ("float64", 1, "band", -9999.0, "same", False, 0.1),
]
FLOAT64_SENTINEL = pytest.mark.xfail(
    strict=True,
    raises=AssertionError,
    reason="known difference: lerc-writer 0.5.2 stores raw the blocks where libLerc's "
    "reconstruction misses MAX_Z_ERROR by f64 rounding (Float64 near a far-off nodata value)",
)
# Lossless Int8 and Byte data, which libLerc codes with Huffman codes where they are smaller.
HUFFMAN_CASES = [
    ("int8", 1, "band", None, "same", False, 0.0),
    ("uint8", 1, "band", None, "same", False, 0.0),
    ("uint8", 1, "band", None, "same", False, 0.5),
    ("int8", 1, "band", -128, "same", False, 0.0),
    ("uint8", 1, "band", 255, "same", False, 0.0),
    ("uint8", 3, "pixel", None, "same", False, 0.0),
    ("uint8", 3, "band", None, "same", False, 0.0),
    ("uint8", 1, "band", None, "same", True, 0.0),
]


def _id(case):
    dt, bands, interleave, nodata, holes, mask, mze = case
    parts = [dt, f"x{bands}"] + ([interleave] if bands > 1 else [])
    if nodata is not None:
        parts.append(f"nodata{nodata}" + ("-differ" if holes == "differ" else ""))
    if mask:
        parts.append("mask")
    parts.append(f"mze{mze:g}")
    return "-".join(parts)


def _params(cases):
    """The cases as pytest parameters, GDAL 3.13's padding rows and the Float64 sentinel rows
    marked."""
    return [
        pytest.param(
            c,
            id=_id(c),
            marks=FLOAT64_SENTINEL,
        )
        if c in FLOAT64_SENTINEL_CASES
        else pytest.param(
            c,
            id=_id(c),
            marks=GDAL_313_PADDING
            if c[0].startswith("float") and c[1] > 1 and c[2] == "pixel"
            else (),
        )
        for c in cases
    ]


def _unzstd(data):
    """Decompresses a Zstandard frame with whichever library is installed."""
    try:
        from compression import zstd  # Python 3.14

        return zstd.decompress(data)
    except ImportError:
        pass
    try:
        import zstandard

        return zstandard.ZstdDecompressor().decompress(data, max_output_size=1 << 26)
    except ImportError:
        pass
    numcodecs = pytest.importorskip(
        "numcodecs", reason="no Zstandard decoder (zstandard, numcodecs)"
    )
    return bytes(numcodecs.Zstd().decode(data))


def _blob(chunk, compress):
    if compress == "LERC_DEFLATE":
        return zlib.decompress(chunk)
    if compress == "LERC_ZSTD":
        return _unzstd(chunk)
    return chunk


def _chunks(path):
    """((level, band, x, y), stored bytes) of every chunk, every band and every overview level."""
    out = []
    with rasterio.open(path) as d:
        levels = [None] + list(range(len(d.overviews(1))))
    for level in levels:
        with rasterio.open(path, overview_level=level) as d, open(path, "rb") as f:
            pixel = d.count > 1 and d.interleaving == rasterio.enums.Interleaving.pixel
            for band in [1] if pixel else range(1, d.count + 1):  # a pixel's tile holds every band
                for ty in range(-(-d.height // TILE)):
                    for tx in range(-(-d.width // TILE)):
                        off = d.get_tag_item(f"BLOCK_OFFSET_{tx}_{ty}", "TIFF", bidx=band)
                        size = d.get_tag_item(f"BLOCK_SIZE_{tx}_{ty}", "TIFF", bidx=band)
                        f.seek(int(off))
                        out.append(((level, band, tx, ty), f.read(int(size))))
    return out


def _write(
    tmp_path,
    a,
    nodata,
    driver,
    compress,
    mze,
    *,
    interleave="band",
    mask=None,
    encoder=None,
    overviews=False,
    extra=None,
):
    """Writes ``a`` with GDAL (rasterio) and Terrio; returns the two paths.

    ``mze`` None leaves MAX_Z_ERROR out; ``extra`` holds other creation options for both.
    """
    gdal_path, terrio_path = tmp_path / "gdal.tif", tmp_path / f"terrio-{encoder}.tif"
    profile = dict(
        driver="GTiff",
        width=W,
        height=H,
        count=a.shape[0],
        dtype=a.dtype,
        nodata=nodata,
        crs="EPSG:32615",
        transform=rasterio.Affine.from_gdal(*GT),
    )
    with MemoryFile() as mem, mem.open(**profile) as src:
        src.write(a)
        if mask is not None:
            src.write_mask(mask)
        co = {"interleave": interleave, **{k.lower(): v for k, v in (extra or {}).items()}}
        if mze is not None:
            co["max_z_error"] = mze
        if driver == "COG":
            co.update(blocksize=TILE)
            co.update(overview_resampling="nearest") if overviews else co.update(overviews="NONE")
        else:
            co.update(tiled=True, blockxsize=TILE, blockysize=TILE)
        rasterio_shutil.copy(src, gdal_path, driver=driver, compress=compress, **co)
    options = {"INTERLEAVE": interleave.upper(), **(extra or {})}
    if mze is not None:
        options["MAX_Z_ERROR"] = str(mze)
    if driver == "COG":
        if not overviews:
            options["OVERVIEWS"] = "NONE"
    else:
        options["TILED"] = "YES"
    if encoder is not None:
        options["LERC_ENCODER"] = encoder
    raster = terrio.Raster.from_array(a, GT, crs="EPSG:32615", nodata=nodata)
    raster.write(
        str(terrio_path),
        driver=driver,
        compress=compress,
        tile_size=TILE,
        options=options,
        mask=mask,
        overview_resampling="nearest" if overviews else None,
    )
    return gdal_path, terrio_path


def _assert_reads_as_gdal_s(gdal_path, terrio_path):
    """The pixels and masks GDAL reads back are the same."""
    with rasterio.open(gdal_path) as g, rasterio.open(terrio_path) as t:
        np.testing.assert_array_equal(t.read(), g.read())
        np.testing.assert_array_equal(t.read_masks(), g.read_masks())


def _assert_blobs_are_gdal_s(gdal_path, terrio_path, compress):
    gdal, ours = _chunks(gdal_path), _chunks(terrio_path)
    assert [k for k, _ in gdal] == [k for k, _ in ours]
    for (key, g), (_, t) in zip(gdal, ours, strict=True):
        assert _blob(g, compress)[:6] == b"Lerc2 ", key
        assert _blob(t, compress) == _blob(g, compress), f"level, band, tile x, y = {key}"
    _assert_reads_as_gdal_s(gdal_path, terrio_path)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("compress", ["LERC", "LERC_DEFLATE", "LERC_ZSTD"])
@pytest.mark.parametrize("case", _params(CASES + FLOAT64_SENTINEL_CASES))
def test_default_lerc_blobs_are_gdal_s(tmp_path, driver, compress, case):
    dt, bands, interleave, nodata, holes, mask, mze = case
    a = _array(dt, bands, nodata, holes)
    paths = _write(
        tmp_path,
        a,
        nodata,
        driver,
        compress,
        mze,
        interleave=interleave,
        mask=_mask() if mask else None,
    )
    _assert_blobs_are_gdal_s(*paths, compress)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("compress", ["LERC", "LERC_DEFLATE", "LERC_ZSTD"])
@pytest.mark.parametrize("case", HUFFMAN_CASES, ids=_id)
def test_lossless_8bit_lerc_reads_as_gdal_s(tmp_path, driver, compress, case):
    """Lossless Int8 and Byte: GDAL reads the same pixels and masks (the blobs differ, below)."""
    dt, bands, interleave, nodata, holes, mask, mze = case
    a = _array(dt, bands, nodata, holes)
    paths = _write(
        tmp_path,
        a,
        nodata,
        driver,
        compress,
        mze,
        interleave=interleave,
        mask=_mask() if mask else None,
    )
    _assert_reads_as_gdal_s(*paths)
    with rasterio.open(paths[1]) as t:
        np.testing.assert_array_equal(t.read(), a)
    # Only the tiles GDAL codes with Huffman codes differ.
    gdal, ours = _chunks(paths[0]), _chunks(paths[1])
    assert [k for k, _ in gdal] == [k for k, _ in ours]
    modes = [_lerc_mode(_blob(g, compress)) for _, g in gdal]
    assert "huffman" in modes
    for (key, g), (_, t), mode in zip(gdal, ours, modes, strict=True):
        if mode != "huffman":
            assert _blob(t, compress) == _blob(g, compress), f"{mode} tile {key}"


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("compress", ["LERC", "LERC_DEFLATE", "LERC_ZSTD"])
@pytest.mark.parametrize("case", FLOAT64_SENTINEL_CASES, ids=_id)
def test_lossy_float64_sentinel_reads_within_the_error(tmp_path, driver, compress, case):
    """Float64 near a far-off nodata value: the tiles without it are GDAL's, the pixels are
    within MAX_Z_ERROR and nodata is kept (the other tiles differ, above)."""
    dt, bands, interleave, nodata, holes, mask, mze = case
    a = _array(dt, bands, nodata, holes)
    paths = _write(tmp_path, a, nodata, driver, compress, mze)
    gdal, ours = _chunks(paths[0]), _chunks(paths[1])
    assert [k for k, _ in gdal] == [k for k, _ in ours]
    for ((_level, band, tx, ty), g), (_, t) in zip(gdal, ours, strict=True):
        tile = a[band - 1, ty * TILE : (ty + 1) * TILE, tx * TILE : (tx + 1) * TILE]
        if not (tile == nodata).any():
            assert _blob(t, compress) == _blob(g, compress), (tx, ty)
    with rasterio.open(paths[1]) as t, rasterio.open(paths[0]) as g:
        back, theirs = t.read().astype("float64"), g.read().astype("float64")
    holes = a == nodata
    np.testing.assert_array_equal(back == nodata, holes)
    assert np.abs(back - a)[~holes].max() <= mze + 1e-9
    assert np.abs(theirs - a)[~holes].max() <= mze + 1e-9


def _lerc_mode(blob):
    """How a Lerc2 v4 blob of 8-bit data codes its pixels: "constant", "one sweep", "tiles" or
    "huffman" (libLerc's IEM_DeltaHuffman or IEM_Huffman)."""
    assert blob[:6] == b"Lerc2 " and struct.unpack_from("<i", blob, 6)[0] == 4
    depth = struct.unpack_from("<i", blob, 22)[0]
    valid, _, _, data_type = struct.unpack_from("<4i", blob, 26)
    assert data_type in (0, 1), data_type  # Char, Byte
    max_z_error, z_min, z_max = struct.unpack_from("<3d", blob, 42)
    pos = 66
    pos += 4 + struct.unpack_from("<i", blob, pos)[0]  # the mask
    if valid == 0 or z_min == z_max:
        return "constant"
    pos += 2 * depth  # each depth's minimum and maximum, one byte each
    if blob[pos]:
        return "one sweep"
    return "huffman" if max_z_error == 0.5 and blob[pos + 1] in (1, 2) else "tiles"


@pytest.mark.xfail(
    strict=True,
    raises=AssertionError,
    reason="known difference: lerc-writer 0.5.2's Huffman coding (a version 2 code table) is not "
    "libLerc's (version 4), used for lossless 8-bit data where it is smaller than tiles",
)
@pytest.mark.parametrize("case", HUFFMAN_CASES[:2], ids=_id)
def test_lossless_8bit_lerc_blobs_are_gdal_s(tmp_path, case):
    dt, bands, interleave, nodata, holes, mask, mze = case
    paths = _write(tmp_path, _array(dt, bands, nodata, holes), nodata, "GTiff", "LERC", mze)
    _assert_blobs_are_gdal_s(*paths, "LERC")


@pytest.mark.parametrize(
    "case",
    [
        ("float32", 1, "band", NAN, "same", False, 0.01),
        ("uint16", 1, "band", 65535, "same", False, 0.0),
        ("uint8", 3, "pixel", None, "same", False, 1.0),
        ("float32", 3, "band", NAN, "differ", False, 0.01),
        ("float32", 1, "band", None, "same", True, 0.01),
    ],
    ids=_id,
)
def test_default_lerc_cog_overview_blobs_are_gdal_s(tmp_path, case):
    """Every level of a COG with nearest-neighbour overviews (300 x 200, 150 x 100, 75 x 50)."""
    dt, bands, interleave, nodata, holes, mask, mze = case
    a = _array(dt, bands, nodata, holes)
    paths = _write(
        tmp_path,
        a,
        nodata,
        "COG",
        "LERC",
        mze,
        interleave=interleave,
        mask=_mask() if mask else None,
        overviews=True,
    )
    with rasterio.open(paths[0]) as d:
        assert len(d.overviews(1)) == 2
    _assert_blobs_are_gdal_s(*paths, "LERC")


def test_compact_lerc_blobs_are_no_larger_within_the_error(tmp_path):
    a = _array("float32", 1, NAN)
    _, gdal_like = _write(tmp_path, a, NAN, "GTiff", "LERC", 0.01, encoder="GDAL")
    _, compact = _write(tmp_path, a, NAN, "GTiff", "LERC", 0.01, encoder="COMPACT")
    assert sum(len(c) for _, c in _chunks(compact)) <= sum(len(c) for _, c in _chunks(gdal_like))
    with rasterio.open(compact) as c:
        back = c.read(1)
    valid = ~np.isnan(a[0])
    assert np.array_equal(np.isnan(back), ~valid)
    # Within MAX_Z_ERROR, up to the Float32 rounding of the stored values (an ulp near 100).
    err = np.abs(back[valid].astype("float64") - a[0][valid].astype("float64")).max()
    assert err <= 0.01 + 1e-5


@pytest.mark.parametrize(("dtype", "mze"), [("uint16", 0.0), ("int16", 1.0)])
def test_compact_lerc_blobs_differ_on_interleaved_integers(tmp_path, dtype, mze):
    """Pixel-interleaved integers are where COMPACT (Lerc2 v5 difference tiles) is smaller."""
    a = _array(dtype, 3)
    _, gdal_like = _write(
        tmp_path, a, None, "GTiff", "LERC", mze, interleave="pixel", encoder="GDAL"
    )
    _, compact = _write(
        tmp_path, a, None, "GTiff", "LERC", mze, interleave="pixel", encoder="COMPACT"
    )
    assert sum(len(c) for _, c in _chunks(compact)) < sum(len(c) for _, c in _chunks(gdal_like))
    with rasterio.open(compact) as c, rasterio.open(gdal_like) as g:
        back = c.read()
        assert np.abs(back.astype("int64") - a.astype("int64")).max() <= mze
        if mze == 0:
            np.testing.assert_array_equal(back, g.read())


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_lerc_encoder_values(tmp_path, driver):
    """GDAL and COMPACT, in any case; other values are refused, naming the option."""
    a = _array("float32", 1, NAN)
    raster = terrio.Raster.from_array(a, GT, nodata=NAN)
    write = lambda name, value: raster.write(  # noqa: E731
        str(tmp_path / name), driver=driver, compress="LERC", options={"LERC_ENCODER": value}
    )
    write("default.tif", "GDAL")
    write("lower.tif", "gdal")
    assert (tmp_path / "default.tif").read_bytes() == (tmp_path / "lower.tif").read_bytes()
    write("compact.tif", " compact ")
    for value in ["FAST", "", "LIBLERC", "GDAL,COMPACT"]:
        with pytest.raises(terrio.TerrioError, match=r"invalid LERC_ENCODER .*\(GDAL, COMPACT\)"):
            write("x.tif", value)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_lerc_version_is_ignored_as_gdal_ignores_it(tmp_path, driver):
    """GDAL's GTiff and COG drivers have no LERC_VERSION creation option (GDAL 3.12.2, 3.13.3).

    They warn ``driver GTiff does not support creation option LERC_VERSION`` and write the
    Lerc2 v4 blobs they always write; LERC_VERSION=2.4 is the IMAGE_STRUCTURE item GDAL reads back
    from the LercParameters tag. Terrio writes the same file, with the same warning.
    """
    a = _array("float32", 1, NAN)
    raster = terrio.Raster.from_array(a, GT, nodata=NAN)
    raster.write(
        str(tmp_path / "plain.tif"), driver=driver, compress="LERC", options={"MAX_Z_ERROR": "0.01"}
    )
    for value in ["2.4", "3", "BOGUS"]:
        with pytest.warns(
            UserWarning, match=f"driver {driver} does not support creation option LERC_VERSION"
        ):
            raster.write(
                str(tmp_path / "v.tif"),
                driver=driver,
                compress="LERC",
                options={"MAX_Z_ERROR": "0.01", "LERC_VERSION": value},
            )
        assert (tmp_path / "v.tif").read_bytes() == (tmp_path / "plain.tif").read_bytes(), value
    with rasterio.open(tmp_path / "plain.tif") as d:
        assert d.tags(ns="IMAGE_STRUCTURE")["LERC_VERSION"] == "2.4"
    # Once per write, and whatever the compression, as GDAL warns.
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        raster.write(
            str(tmp_path / "d.tif"),
            driver=driver,
            compress="DEFLATE",
            options={"LERC_VERSION": "2.4"},
        )
    assert [str(w.message) for w in caught] == [
        f"driver {driver} does not support creation option LERC_VERSION"
    ]
    # Not without it.
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        raster.write(str(tmp_path / "plain.tif"), driver=driver, compress="LERC")


def _gdal_metadata(path):
    """The GDAL_METADATA tag (42112) of a little-endian classic TIFF's first image."""
    b = path.read_bytes()
    assert b[:4] == b"II*\x00"
    ifd = struct.unpack_from("<I", b, 4)[0]
    for i in range(struct.unpack_from("<H", b, ifd)[0]):
        tag, _, count, value = struct.unpack_from("<HHII", b, ifd + 2 + 12 * i)
        if tag == 42112:
            start = value if count > 4 else ifd + 10 + 12 * i
            return b[start : start + count].rstrip(b"\x00").decode()
    return None


@pytest.mark.parametrize(
    ("mze", "mzeo"), [("0.01", "0.1"), ("0", "0.05"), ("0.1", "0"), ("0.01", "0.01")]
)
@pytest.mark.parametrize("compress", ["LERC", "LERC_ZSTD"])
def test_max_z_error_overview_is_gdal_s(tmp_path, compress, mze, mzeo):
    """A COG's overviews at MAX_Z_ERROR_OVERVIEW: every level's blob, and GDAL_METADATA."""
    a = _array("float32", 1, NAN)
    paths = _write(
        tmp_path, a, NAN, "COG", compress, mze, overviews=True, extra={"MAX_Z_ERROR_OVERVIEW": mzeo}
    )
    _assert_blobs_are_gdal_s(*paths, compress)
    assert _gdal_metadata(paths[1]) == _gdal_metadata(paths[0]).rstrip("\n")
    with rasterio.open(paths[1], overview_level=0) as t:
        overview = t.read(1).astype("float64")
    expected = a[0][::2, ::2].astype("float64")
    valid = ~np.isnan(expected)
    assert np.abs(overview[valid] - expected[valid]).max() <= float(mzeo) + 1e-5


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize(
    ("mze", "mzeo"), [("0.01", "0.1"), ("0", "0.05"), (None, "0.1"), ("0.0", "0")]
)
def test_max_z_error_overview_metadata_is_gdal_s(tmp_path, driver, mze, mzeo):
    """Recorded with MAX_Z_ERROR as given (empty when it is not) when the two differ, overviews
    or not, as GDAL records them."""
    a = _array("float32", 1, NAN)
    paths = _write(tmp_path, a, NAN, driver, "LERC", mze, extra={"MAX_Z_ERROR_OVERVIEW": mzeo})
    assert _gdal_metadata(paths[1]) == _gdal_metadata(paths[0]).rstrip("\n")


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize(
    ("compress", "mze"),
    [("LERC", 0.01), ("LERC", 0.0), ("LERC_DEFLATE", 0.001), ("LERC_ZSTD", 0.5)],
)
def test_lerc_metadata_is_gdal_s(tmp_path, driver, compress, mze):
    # GDAL records MAX_Z_ERROR as given, or COMPRESSION_REVERSIBILITY=LOSSLESS.
    a = _array("float32", 1, NAN)
    gdal_path, terrio_path = _write(tmp_path, a, NAN, driver, compress, mze)
    # GDAL's serialization ends with a newline, terrio's GDAL_METADATA does not, whatever the
    # compression (a separate, known difference).
    assert _gdal_metadata(terrio_path) == _gdal_metadata(gdal_path).rstrip("\n")
    with rasterio.open(gdal_path) as g, rasterio.open(terrio_path) as t:
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")
