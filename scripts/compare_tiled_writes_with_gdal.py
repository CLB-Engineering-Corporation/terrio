# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""GeoTIFF tiles written in any order, and `SPARSE_OK`, against GDAL (reference GDAL 3.13.3).

Tile order: terrio's `GeoTiffWriter.write_tile` writes the tiles of a raster whose right and
bottom tiles are cut by its edges in a shuffled order; GDAL's GTiff driver writes the same tiles,
in the same order, to a file of the same creation options (`Create`, then one `WriteRaster` per
tile). GDAL reads both: samples bit for bit, data type, block size, compression, predictor,
interleaving, nodata, geotransform and CRS must be the same. Byte, Int16, Int32, Float32 and
Float64 rasters, LZW, DEFLATE and ZSTD, predictors 1, 2 and 3 (floating point), pixel and band
interleaving.

Tiles not given: without `SPARSE_OK`, both fill them with nodata (or 0), and every block has bytes;
with `SPARSE_OK=YES`, both leave them out, and the tiles given whose samples are all nodata (or 0):
the same blocks have an offset of 0 (GDAL's `BLOCK_OFFSET_x_y` of the `TIFF` domain), and the files
read the same.

`SPARSE_OK` through the command line: `terrio translate --co SPARSE_OK=YES` and
`gdal_translate -co SPARSE_OK=TRUE` of the same source leave out the same blocks; uncompressed,
the blocks kept hold the same bytes, and the file sizes are reported. Compressed bytes are not
compared. No data is downloaded or modified.
"""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

import numpy as np
from osgeo import gdal, osr

import terrio

TRANSFORM = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
# 5 x 4 tiles of 64, the last column 44 pixels wide and the last row 25 high.
WIDTH, HEIGHT, TILE = 300, 217, 64
GDAL_TYPES = {
    "uint8": gdal.GDT_Byte,
    "int16": gdal.GDT_Int16,
    "int32": gdal.GDT_Int32,
    "float32": gdal.GDT_Float32,
    "float64": gdal.GDT_Float64,
}


def tile_cases():
    """(name, dtype, bands, creation options) of the tile-order comparisons."""
    for dtype in GDAL_TYPES:
        predictors = ["1", "2", "3"] if dtype.startswith("float") else ["1", "2"]
        for compress in ("LZW", "DEFLATE", "ZSTD"):
            for predictor in predictors:
                co = {"COMPRESS": compress, "PREDICTOR": predictor}
                yield f"{dtype} {compress} PREDICTOR={predictor}", dtype, 2, co
        co = {"COMPRESS": "DEFLATE", "PREDICTOR": "2", "INTERLEAVE": "BAND"}
        yield f"{dtype} 3 bands DEFLATE PREDICTOR=2 INTERLEAVE=BAND", dtype, 3, co


def source(dtype, bands, nodata, seed):
    """A surface with noise, a NaN in floating-point rasters, and one tile of nodata."""
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:HEIGHT, 0:WIDTH]
    planes = [np.sin(x / (17.0 + b)) * np.cos(y / 23.0) * 100 + rng.random((HEIGHT, WIDTH)) * 3 for b in range(bands)]
    out = np.stack(planes)
    if np.dtype(dtype).kind != "f":
        out = np.clip(np.round(out), 0 if dtype == "uint8" else -1000, 250 if dtype == "uint8" else 1000)
    out = out.astype(dtype)
    if np.dtype(dtype).kind == "f":
        out[0, 100, 7] = np.nan
    out[:, TILE : 2 * TILE, TILE : 2 * TILE] = nodata
    return out


def shuffled_tiles(seed, keep=None):
    across, down = -(-WIDTH // TILE), -(-HEIGHT // TILE)
    tiles = [(c, r) for r in range(down) for c in range(across)]
    np.random.default_rng(seed).shuffle(tiles)
    return [t for t in tiles if keep is None or keep(t)]


def window(c, r):
    x, y = c * TILE, r * TILE
    return x, y, min(TILE, WIDTH - x), min(TILE, HEIGHT - y)


def srs_wkt():
    srs = osr.SpatialReference()
    srs.ImportFromEPSG(32610)
    return srs.ExportToWkt()


def write_terrio(path, data, nodata, co, tiles):
    bands = data.shape[0]
    options = {"TILED": "YES", "BLOCKXSIZE": str(TILE), "BLOCKYSIZE": str(TILE), **co}
    compress = options.pop("COMPRESS")
    w = terrio.GeoTiffWriter(
        str(path), WIDTH, HEIGHT, bands, data.dtype.name, TRANSFORM, "EPSG:32610", nodata, compress=compress, options=options
    )
    for c, r in tiles:
        x, y, tw, th = window(c, r)
        w.write_tile(c, r, data[:, y : y + th, x : x + tw])
    assert w.tiles_written == len(tiles)
    w.close()


def write_gdal(path, data, nodata, co, tiles):
    bands = data.shape[0]
    options = {"TILED": "YES", "BLOCKXSIZE": str(TILE), "BLOCKYSIZE": str(TILE), **co}
    ds = gdal.GetDriverByName("GTiff").Create(
        str(path), WIDTH, HEIGHT, bands, GDAL_TYPES[data.dtype.name], [f"{k}={v}" for k, v in options.items()]
    )
    ds.SetGeoTransform(TRANSFORM)
    ds.SetProjection(srs_wkt())
    for b in range(bands):
        ds.GetRasterBand(b + 1).SetNoDataValue(nodata)
    for c, r in tiles:
        x, y, tw, th = window(c, r)
        block = np.ascontiguousarray(data[:, y : y + th, x : x + tw])
        ds.WriteRaster(x, y, tw, th, block.tobytes(), tw, th, GDAL_TYPES[data.dtype.name], list(range(1, bands + 1)))
    ds = None


def structure(path):
    """What GDAL reads of a file: samples, and everything but its compressed bytes."""
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    ims = ds.GetMetadata("IMAGE_STRUCTURE")
    srs = ds.GetSpatialRef()
    meta = dict(
        type=gdal.GetDataTypeName(band.DataType),
        block=band.GetBlockSize(),
        compression=ims.get("COMPRESSION"),
        predictor=ims.get("PREDICTOR", "1"),
        interleave=ims.get("INTERLEAVE"),
        nodata=[ds.GetRasterBand(b + 1).GetNoDataValue() for b in range(ds.RasterCount)],
        transform=ds.GetGeoTransform(),
        epsg=srs.GetAuthorityCode(None) if srs else None,
    )
    return ds.ReadAsArray(), meta, omitted_blocks(ds)


def omitted_blocks(ds):
    """The blocks of each band with no bytes (`SPARSE_OK`)."""
    out = []
    for b in range(ds.RasterCount):
        band = ds.GetRasterBand(b + 1)
        bw, bh = band.GetBlockSize()
        for r in range(-(-ds.RasterYSize // bh)):
            for c in range(-(-ds.RasterXSize // bw)):
                offset = band.GetMetadataItem(f"BLOCK_OFFSET_{c}_{r}", "TIFF")
                if offset in (None, "", "0"):
                    out.append((b + 1, c, r))
    return out


def same_samples(a, b):
    a, b = np.asarray(a), np.asarray(b)
    if a.shape != b.shape or a.dtype != b.dtype:
        return False
    return a.tobytes() == b.tobytes()


def compare_tiles(name, dtype, bands, co, directory, keep=None, sparse=False):
    nodata = 0 if dtype == "uint8" else -9999
    data = source(dtype, bands, nodata, seed=len(name))
    tiles = shuffled_tiles(seed=len(name), keep=keep)
    if sparse:
        co = {**co, "SPARSE_OK": "YES"}
    ours, theirs = directory / "terrio.tif", directory / "gdal.tif"
    write_terrio(ours, data, nodata, co, tiles)
    write_gdal(theirs, data, nodata, {k: ("TRUE" if k == "SPARSE_OK" else v) for k, v in co.items()}, tiles)
    a, meta_a, omitted_a = structure(ours)
    b, meta_b, omitted_b = structure(theirs)
    if meta_a != meta_b:
        raise ValueError(f"structure differs: terrio {meta_a}, GDAL {meta_b}")
    if not same_samples(a, b):
        raise ValueError(f"samples differ in {int(np.sum(a != b))} pixels")
    if keep is None and not same_samples(a.reshape(data.shape), data):
        raise ValueError("samples differ from the tiles written")
    if omitted_a != omitted_b:
        raise ValueError(f"blocks left out differ: terrio {omitted_a}, GDAL {omitted_b}")
    detail = f"{len(tiles)} tiles in shuffled order, edge tiles of {WIDTH % TILE} x {HEIGHT % TILE}; samples bit for bit, "
    detail += f"{meta_a['type']}, {meta_a['compression']}, PREDICTOR={meta_a['predictor']}, {meta_a['interleave']}"
    if keep is not None or sparse:
        detail += f"; {len(omitted_a)} block(s) left out by both"
    return dict(name=name, status="identical", detail=detail)


def run_cli(cmd):
    """Runs `cmd`, raising its exit status and stderr when it fails."""
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"{Path(cmd[0]).name} {cmd[1]} exited with status {result.returncode}: {result.stderr.strip()}")
    return result


def compare_translate(name, terrio_cli, co, directory):
    """`terrio translate --co SPARSE_OK=YES` against `gdal_translate -co SPARSE_OK=TRUE`."""
    nodata = -9999.0
    data = source("float32", 1, nodata, seed=3)
    # More blocks of nodata: the first tile row, and a tile cut by the bottom edge.
    data[:, :TILE] = nodata
    data[:, 3 * TILE :, 2 * TILE : 3 * TILE] = nodata
    dense = directory / "dense.tif"
    terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:32610", nodata=nodata).write(str(dense), compress="NONE")
    options = {"TILED": "YES", "BLOCKXSIZE": str(TILE), "BLOCKYSIZE": str(TILE), **co}
    ours, theirs = directory / "terrio.tif", directory / "gdal.tif"
    cmd = [terrio_cli, "translate", str(dense), str(ours)]
    for k, v in {**options, "SPARSE_OK": "YES"}.items():
        cmd += ["--co", f"{k}={v}"]
    run_cli(cmd)
    gdal.Translate(str(theirs), str(dense), creationOptions=[f"{k}={v}" for k, v in {**options, "SPARSE_OK": "TRUE"}.items()])
    a, meta_a, omitted_a = structure(ours)
    b, meta_b, omitted_b = structure(theirs)
    if meta_a != meta_b:
        raise ValueError(f"structure differs: terrio {meta_a}, GDAL {meta_b}")
    if not same_samples(a, b):
        raise ValueError(f"samples differ in {int(np.sum(a != b))} pixels")
    if omitted_a != omitted_b or not omitted_a:
        raise ValueError(f"blocks left out differ: terrio {omitted_a}, GDAL {omitted_b}")
    sizes = (ours.stat().st_size, theirs.stat().st_size)
    detail = f"{len(omitted_a)} block(s) left out by both; samples bit for bit; files of {sizes[0]} (terrio) and {sizes[1]} (GDAL) bytes"
    if co.get("COMPRESS") == "NONE":
        kept = [blocks_bytes(p) for p in (ours, theirs)]
        if kept[0] != kept[1]:
            raise ValueError(f"the blocks kept hold {kept[0]} (terrio) and {kept[1]} (GDAL) bytes")
        detail += f", of which {kept[0]} in the blocks kept by both"
    return dict(name=name, status="identical", detail=detail)


def blocks_bytes(path):
    ds = gdal.Open(str(path))
    band = ds.GetRasterBand(1)
    bw, bh = band.GetBlockSize()
    total = 0
    for r in range(-(-ds.RasterYSize // bh)):
        for c in range(-(-ds.RasterXSize // bw)):
            total += int(band.GetMetadataItem(f"BLOCK_SIZE_{c}_{r}", "TIFF") or 0)
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", default=shutil.which("terrio") or "target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    cases = [(name, lambda d, n=name, a=a: compare_tiles(n, *a, d)) for name, *a in tile_cases()]
    # Tiles not given: every fourth.
    missing = lambda t: (t[0] + 2 * t[1]) % 4 != 1  # noqa: E731
    for dtype in ("uint8", "float32"):
        name = f"{dtype} tiles not given, filled"
        cases.append((name, lambda d, n=name, t=dtype: compare_tiles(n, t, 2, {"COMPRESS": "DEFLATE"}, d, keep=missing)))
        for interleave in ("PIXEL", "BAND"):
            name = f"{dtype} SPARSE_OK=YES, tiles not given and tiles of nodata, INTERLEAVE={interleave}"
            co = {"COMPRESS": "DEFLATE", "INTERLEAVE": interleave}
            cases.append((name, lambda d, n=name, t=dtype, co=co: compare_tiles(n, t, 2, co, d, keep=missing, sparse=True)))
    for compress in ("NONE", "DEFLATE"):
        name = f"translate --co SPARSE_OK, COMPRESS={compress}"
        cases.append((name, lambda d, n=name, c=compress: compare_translate(n, args.terrio, {"COMPRESS": c}, d)))
    rows = []
    for name, run in cases:
        with tempfile.TemporaryDirectory(prefix="terrio-tiles-") as directory:
            try:
                rows.append(run(Path(directory)))
            except Exception as error:
                rows.append(dict(name=name, status="differs", detail=str(error)))
    for row in rows:
        if row["status"] == "identical":
            row["detail"] += f"; GDAL {gdal.VersionInfo('RELEASE_NAME')}"
    print(json.dumps(rows, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}: {r['detail']}" for r in rows))
    return int(any(r["status"] == "differs" for r in rows))


if __name__ == "__main__":
    raise SystemExit(main())
