# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""GeoTIFF ``BIGTIFF`` creation options against GDAL's GTiff driver (reference GDAL 3.13.3).

Each large case creates the same empty image with GDAL (``Create`` with ``SPARSE_OK=TRUE``, as
``gdal_create`` does) and with ``terrio create-raster --co SPARSE_OK=TRUE``, at the sizes where
GDAL's choice changes: 4.2 GB of padded tiles for ``IF_NEEDED`` and ``NO`` (uncompressed only),
2 GB for ``IF_SAFER``, strips unpadded. The files must both be classic TIFFs, both BigTIFFs, or
both refused; GDAL then reads terrio's file (its size, bands, type, block size and the first
block's zeros). Blocks of zeros are left out, so no case writes more than a few megabytes.
One case is a known difference, reported as not compared: with compression, ``IF_NEEDED``
makes a BigTIFF of rows written as they come once their samples exceed 3.4 GB (a quarter more
could exceed 4 GiB), where GDAL writes a classic TIFF that fails if it grows past 4 GiB.

The small cases write a Float32 raster with every policy, compressed or not, as a GeoTIFF and a
COG: classic unless ``BIGTIFF=YES``, as GDAL writes it, and read back by GDAL sample for sample.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from osgeo import gdal

TYPES = {"Byte": gdal.GDT_Byte, "Float32": gdal.GDT_Float32}

# (name, width, height, bands, type, creation options): GDAL 3.13.3's choice changes between
# consecutive cases of a pair.
LARGE = [
    ("IF_NEEDED, NONE, 4.18 GB of tiles", 100_000, 41_728, 1, "Byte", ["TILED=YES", "BIGTIFF=IF_NEEDED"]),
    ("IF_NEEDED, NONE, 4.20 GB of tiles", 100_000, 41_729, 1, "Byte", ["TILED=YES", "BIGTIFF=IF_NEEDED"]),
    ("default, NONE, 4.20 GB of tiles", 100_000, 41_729, 1, "Byte", ["TILED=YES"]),
    ("IF_NEEDED, NONE, 3.82 GB of tiles", 100_000, 38_000, 1, "Byte", ["TILED=YES", "BIGTIFF=IF_NEEDED"]),
    ("IF_NEEDED, DEFLATE, 4.20 GB of tiles", 100_000, 41_729, 1, "Byte", ["TILED=YES", "COMPRESS=DEFLATE", "BIGTIFF=IF_NEEDED"]),
    ("NO, NONE, 4.18 GB of tiles", 100_000, 41_728, 1, "Byte", ["TILED=YES", "BIGTIFF=NO"]),
    ("NO, NONE, 4.20 GB of tiles", 100_000, 41_729, 1, "Byte", ["TILED=YES", "BIGTIFF=NO"]),
    ("NO, DEFLATE, 4.20 GB of tiles", 100_000, 41_729, 1, "Byte", ["TILED=YES", "COMPRESS=DEFLATE", "BIGTIFF=NO"]),
    ("IF_SAFER, NONE, 2.00 GB of tiles", 100_000, 19_968, 1, "Byte", ["TILED=YES", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, NONE, 2.02 GB of tiles", 100_000, 19_969, 1, "Byte", ["TILED=YES", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, DEFLATE, 2.00 GB of tiles", 100_000, 19_968, 1, "Byte", ["TILED=YES", "COMPRESS=DEFLATE", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, DEFLATE, 2.02 GB of tiles", 100_000, 19_969, 1, "Byte", ["TILED=YES", "COMPRESS=DEFLATE", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, strips of 2.00 GB", 100_000, 20_000, 1, "Byte", ["BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, strips of 2.0001 GB", 100_000, 20_001, 1, "Byte", ["BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, 4 bands, 2.00 GB", 50_000, 10_000, 4, "Byte", ["COMPRESS=LZW", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, 4 bands, 2.0002 GB", 50_000, 10_001, 4, "Byte", ["COMPRESS=LZW", "BIGTIFF=IF_SAFER"]),
    ("IF_SAFER, Float32 tiles, 2.02 GB", 25_000, 20_001, 1, "Float32", ["TILED=YES", "COMPRESS=DEFLATE", "BIGTIFF=IF_SAFER"]),
]


def version(path):
    """The TIFF version of a file: 42 (classic) or 43 (BigTIFF)."""
    with open(path, "rb") as f:
        head = f.read(4)
    return int.from_bytes(head[2:4], "little" if head[:2] == b"II" else "big")


def gdal_create(path, width, height, bands, dtype, options):
    """The TIFF version of GDAL's file, or None when GDAL refuses it."""
    try:
        ds = gdal.GetDriverByName("GTiff").Create(str(path), width, height, bands, TYPES[dtype], options + ["SPARSE_OK=TRUE"])
    except RuntimeError:
        return None
    if ds is None:
        return None
    ds = None
    return version(path)


def terrio_create(terrio, path, width, height, bands, dtype, options):
    """The TIFF version of terrio's file and its error, the version None when it is refused."""
    co = [a for o in options + ["SPARSE_OK=TRUE"] for a in ("--co", o)]
    cmd = [terrio, "create-raster", str(path), "--size", f"{width},{height}", "--band-count", str(bands), "--ot", dtype, "--overwrite", *co]
    p = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if p.returncode:
        return None, p.stderr.strip().splitlines()[-1] if p.stderr.strip() else f"exit {p.returncode}"
    return version(path), ""


def read_back(path, width, height, bands, dtype, options):
    """GDAL's differences reading terrio's empty file."""
    ds = gdal.Open(str(path))
    diffs = []
    if (ds.RasterXSize, ds.RasterYSize, ds.RasterCount) != (width, height, bands):
        diffs.append(f"size {ds.RasterXSize} x {ds.RasterYSize} x {ds.RasterCount}")
    band = ds.GetRasterBand(1)
    if gdal.GetDataTypeName(band.DataType) != dtype:
        diffs.append(f"type {gdal.GetDataTypeName(band.DataType)}")
    bx, by = band.GetBlockSize()
    if "TILED=YES" in options and (bx, by) != (256, 256):
        diffs.append(f"block {bx} x {by}")
    if np.any(band.ReadAsArray(0, 0, min(bx, width), min(by, height))):
        diffs.append("the first block is not zeros")
    return diffs


# The cases where terrio chooses otherwise by design: (GDAL's TIFF version, terrio's).
KNOWN = {
    "IF_NEEDED, DEFLATE, 4.20 GB of tiles": (
        (42, 43),
        "known difference: terrio writes a BigTIFF since the compressed rows could exceed 4 GiB; GDAL a classic TIFF",
    ),
}


def large_case(terrio, directory, name, width, height, bands, dtype, options):
    expected = gdal_create(directory / "gdal.tif", width, height, bands, dtype, options)
    actual, error = terrio_create(terrio, directory / "terrio.tif", width, height, bands, dtype, options)
    kind = {None: "refused", 42: "classic TIFF", 43: "BigTIFF"}
    known, why = KNOWN.get(name, (None, ""))
    if known == (expected, actual):
        return dict(name=name, status="not compared", detail=why)
    if actual != expected:
        detail = f"GDAL: {kind[expected]}, terrio: {kind[actual]} {error}".strip()
        return dict(name=name, status="differs", detail=detail)
    diffs = [] if actual is None else read_back(directory / "terrio.tif", width, height, bands, dtype, options)
    if diffs:
        return dict(name=name, status="differs", detail="; ".join(diffs))
    size = "" if actual is None else f", {(directory / 'terrio.tif').stat().st_size} bytes"
    return dict(name=name, status="identical", detail=f"{kind[actual]} as GDAL{size}")


def small_cases(terrio, directory):
    rng = np.random.default_rng(7)
    values = rng.normal(100, 30, (203, 301)).astype("float32")
    src = directory / "src.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(src), 301, 203, 1, gdal.GDT_Float32)
    ds.SetGeoTransform((500000, 10, 0, 4000000, 0, -10))
    ds.GetRasterBand(1).WriteArray(values)
    ds = None
    rows = []
    for fmt in ("GTiff", "COG"):
        for compress in ("NONE", "DEFLATE"):
            for policy in ("YES", "NO", "IF_NEEDED", "IF_SAFER"):
                name = f"{fmt}, {compress}, BIGTIFF={policy}, 301 x 203 Float32"
                g_path, t_path = directory / "gdal_small.tif", directory / "terrio_small.tif"
                gdal.Translate(str(g_path), str(src), format=fmt, creationOptions=[f"COMPRESS={compress}", f"BIGTIFF={policy}"])
                cmd = [terrio, "convert", str(src), str(t_path), "-f", fmt, "--compress", compress, "--co", f"BIGTIFF={policy}"]
                p = subprocess.run(cmd, capture_output=True, text=True, check=False)
                if p.returncode:
                    rows.append(dict(name=name, status="differs", detail=f"terrio: {p.stderr.strip()}"))
                    continue
                diffs = []
                if version(t_path) != version(g_path):
                    diffs.append(f"TIFF version {version(t_path)}, GDAL's {version(g_path)}")
                read = gdal.Open(str(t_path)).ReadAsArray()
                if not np.array_equal(read, values):
                    diffs.append("samples differ")
                kind = "BigTIFF" if version(t_path) == 43 else "classic TIFF"
                status = "differs" if diffs else "identical"
                rows.append(dict(name=name, status=status, detail="; ".join(diffs) or f"{kind} as GDAL; exact samples"))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", default=os.environ.get("TERRIO_BIN", "terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        directory = Path(tmp)
        for name, width, height, bands, dtype, options in LARGE:
            rows.append(large_case(args.terrio, directory, name, width, height, bands, dtype, options))
        rows += small_cases(args.terrio, directory)
    if args.json:
        print(json.dumps(rows))
    else:
        for row in rows:
            print(f"{row['status']:>9}  {row['name']}: {row['detail']}")
    return 0 if all(row["status"] != "differs" for row in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
