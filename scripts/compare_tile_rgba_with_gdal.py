"""Exact independent GDAL 3.13 tile reprojection/overview reference.

Run with lwi-gdal Python and --terrio PATH. No GDAL CLI is used.
"""

import argparse
import io
import json
import sqlite3
import subprocess
from pathlib import Path
import numpy as np
from osgeo import gdal, osr
from PIL import Image


def decoded(path):
    with sqlite3.connect(path) as db:
        return {
            (z, x, y): np.asarray(Image.open(io.BytesIO(data)).convert("RGBA"))
            for z, x, y, data in db.execute(
                "select zoom_level,tile_column,tile_row,tile_data from tiles"
            )
        }


def compare(a, b):
    keys = a.keys() & b.keys()
    return dict(
        coverage_equal=a.keys() == b.keys(),
        tiles=len(a),
        channels=sum(int(np.count_nonzero(a[k] != b[k])) for k in keys),
        alpha=sum(int(np.count_nonzero(a[k][..., 3] != b[k][..., 3])) for k in keys),
        maxdelta=max(
            (int(np.abs(a[k].astype(int) - b[k].astype(int)).max()) for k in keys),
            default=0,
        ),
    )


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--terrio", required=True)
    p.add_argument(
        "--strict", action="store_true", help="Fail on any full RGBA mismatch"
    )
    p.add_argument("--output", default="target/evidence/tile-rgba")
    args = p.parse_args()
    args.terrio = str(Path(args.terrio).resolve())
    assert gdal.VersionInfo().startswith("313"), gdal.VersionInfo()
    gdal.UseExceptions()
    root = Path(args.output)
    root.mkdir(parents=True, exist_ok=True)
    results = []
    for name in ["opaque", "binary", "variable-alpha", "offgrid"]:
        w, h = 513, 377
        yy, xx = np.indices((h, w))
        rgb = np.stack(
            [
                (xx * 17 + yy * 3) % 256,
                (xx * 5 + yy * 23) % 256,
                (xx * 7 + yy * 11) % 256,
            ]
        ).astype("uint8")
        alpha = np.full((h, w), 255, dtype="uint8")
        if name == "binary":
            alpha[(xx + yy) % 7 == 0] = 0
        if name == "variable-alpha":
            alpha = np.array([0, 64, 127, 255], dtype="uint8")[(xx + yy) % 4]
        rgba = np.concatenate([rgb, alpha[None]])
        src = root / (name + ".tif")
        ds = gdal.GetDriverByName("GTiff").Create(str(src), w, h, 4, gdal.GDT_Byte)
        srs = osr.SpatialReference()
        srs.ImportFromEPSG(4326)
        ds.SetProjection(srs.ExportToWkt())
        ds.SetGeoTransform(
            (
                -88.1234567 + (0.000031 if name == "offgrid" else 0),
                0.0001,
                0,
                33.1234567,
                0,
                -0.0001,
            )
        )
        for i in range(4):
            ds.GetRasterBand(i + 1).WriteArray(rgba[i])
            ds.GetRasterBand(i + 1).SetColorInterpretation(
                [
                    gdal.GCI_RedBand,
                    gdal.GCI_GreenBand,
                    gdal.GCI_BlueBand,
                    gdal.GCI_AlphaBand,
                ][i]
            )
        ds = None
        ref = root / (name + "-gdal.mbtiles")
        ref.unlink(missing_ok=True)
        ds = gdal.Translate(
            str(ref),
            str(src),
            format="MBTiles",
            creationOptions=["RESAMPLING=BILINEAR", "TILE_FORMAT=PNG"],
        )
        ds = None
        base = decoded(ref)
        ds = gdal.Open(str(ref), gdal.GA_Update)
        factors = [
            2**i for i in range(1, 6) if ds.GetRasterBand(1).GetOverviewCount() >= i
        ]
        ds.BuildOverviews("AVERAGE", factors)
        ds = None
        full = decoded(ref)
        for version in ["default"]:
            out = root / (name + "-" + version + ".mbtiles")
            out.unlink(missing_ok=True)
            subprocess.run(
                [
                    args.terrio,
                    "convert",
                    str(src),
                    str(out),
                    "--co",
                    "RESAMPLING=BILINEAR",
                    "--co",
                    "OVERVIEW_RESAMPLING=AVERAGE",
                    "--co",
                    "MINZOOM=" + str(min(k[0] for k in full)),
                ],
                check=True,
                capture_output=True,
            )
            actual = decoded(out)
            z = max(k[0] for k in base)
            results.append(
                dict(
                    case=name,
                    version=version,
                    base=compare(base, {k: v for k, v in actual.items() if k[0] == z}),
                    pyramid=compare(full, actual),
                )
            )
    print(json.dumps(dict(gdal=gdal.VersionInfo(), results=results), indent=2))
    if args.strict and any(
        not r["pyramid"]["coverage_equal"] or r["pyramid"]["channels"] for r in results
    ):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
