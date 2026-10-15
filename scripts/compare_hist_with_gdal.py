"""Compare ``terrio info --hist`` with ``gdalinfo -hist`` on real rasters of several data types.

Usage:
    python scripts/compare_hist_with_gdal.py DATA_DIR [--terrio target/release/terrio] [--json]

Each raster is copied (with its sidecar files, without any ``.aux.xml``) into a temporary
directory, then compared in four ways:

- ``text``: the histogram lines of ``gdalinfo -hist`` (its approximate minimum and maximum)
  against ``terrio info --hist``, byte for byte;
- ``json``: the ``histogram`` objects of ``gdalinfo -json -hist`` against ``terrio info --json
  --hist`` (count, buckets, and ``min``/``max`` as GDAL prints them);
- ``stats``: ``gdalinfo -stats -hist`` (exact minimum and maximum) against ``terrio info --stats
  --hist``;
- ``stored``: after ``gdalinfo -hist`` saved its histograms in the ``.aux.xml`` sidecar, terrio
  must read them back.

Needs GDAL's command-line tools and a built ``terrio`` binary. Prints one line per case, and with
``--json`` a JSON list of results; exits non-zero when any case differs.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

# Rasters of several data types and layouts: GeoTIFFs (strips, tiles, overviews, statistics in
# their metadata) and formats GDAL reads by scanline (some with a minimum and maximum of their own).
FILES = [
    "landsat.tif",  # Byte, 4 bands, tiles and overviews
    "nlcd_knoxville.tif",  # Byte, palette, strips
    "srtm_sub.tif",  # Int16, strips
    "srtm90.tif",  # Int16, tiles and overviews
    "las_vegas_dem.tif",  # Float32, -inf nodata
    "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",  # Float32, NaN nodata
    "usgs_dem.tif",  # Float32, statistics in the GeoTIFF metadata
    "cog.tif",  # Float64, 4 bands
    "dem.tif",  # Float64, statistics and overviews
    "srtm_sub.bil",  # EHdr, with a .stx file
    "srtm_sub.rst",  # Idrisi, minimum and maximum in the .rdc
    "srtm_sub.grd",  # Surfer, Z range in the header
    "srtm_sub.sdat",  # SAGA
    "srtm_sub.ers",  # ER Mapper
    "srtm_sub.nc",  # netCDF
    "lv_dem.asc",  # Esri ASCII grid
    "lv_dem.envi",  # ENVI
]

# Types derived from a DEM with gdal_translate (GeoTIFF strips), for the types the files lack.
DERIVED = [("srtm_sub_uint16.tif", "UInt16"), ("srtm_sub_int32.tif", "Int32"), ("srtm_sub_uint32.tif", "UInt32"), ("srtm_sub_int8.tif", "Int8")]


def run(cmd, env=None):
    """Runs a command and returns its standard output.

    Args:
        cmd: The command line.
        env: Extra environment variables.

    Returns:
        The output, or raises RuntimeError with the error output.
    """
    full_env = dict(os.environ)
    full_env.update(env or {})
    p = subprocess.run(cmd, capture_output=True, text=True, env=full_env)
    if p.returncode != 0:
        raise RuntimeError(p.stderr.strip()[-300:])
    return p.stdout


def text_histograms(out):
    """The histogram lines of ``gdalinfo -hist`` or ``terrio info --hist`` output.

    Args:
        out: The tool's output.

    Returns:
        A list of ``(header, counts)`` line pairs.
    """
    lines = out.splitlines()
    return [(lines[i], lines[i + 1]) for i, line in enumerate(lines) if re.match(r"^  \d+ buckets from ", line) and i + 1 < len(lines)]


def json_histograms(out):
    """The histogram objects of a JSON summary, with ``min`` and ``max`` as printed.

    Args:
        out: The JSON output of ``gdalinfo -json -hist`` or ``terrio info --json --hist``.

    Returns:
        A list of ``(count, min text, max text, buckets)``.
    """
    texts = re.findall(r'"histogram":\s*\{\s*"count":\s*(\d+),\s*"min":\s*([^,\s]+),\s*"max":\s*([^,\s]+)', out)
    bands = json.loads(out)["bands"]
    buckets = [b["histogram"]["buckets"] for b in bands if "histogram" in b]
    return [(int(c), lo, hi, b) for (c, lo, hi), b in zip(texts, buckets)]


def copy_dataset(data, name, tmp):
    """Copies a dataset and its sidecar files (not ``.aux.xml``) into a directory.

    Args:
        data: The data directory.
        name: The dataset's file name.
        tmp: The directory to copy into.

    Returns:
        The copy's path.
    """
    stem = os.path.splitext(name)[0]
    for f in os.listdir(data):
        sidecar = f == stem or (f.startswith(stem + ".") and not f.endswith(".aux.xml"))
        if (f == name or sidecar) and os.path.isfile(os.path.join(data, f)):
            shutil.copy(os.path.join(data, f), tmp)
    return os.path.join(tmp, name)


def compare(path, terrio):
    """Compares one raster's histograms.

    Args:
        path: The raster (a copy without sidecar statistics).
        terrio: The terrio executable.

    Returns:
        A list of ``(case, differences)``.
    """
    no_pam = {"GDAL_PAM_ENABLED": "NO"}
    out = []
    t = text_histograms(run([terrio, "info", "--hist", path]))
    g = text_histograms(run(["gdalinfo", "-hist", path], no_pam))
    out.append(("text", [] if t == g else [f"{len(t)} vs {len(g)} histograms, first difference: {next((a, b) for a, b in zip(t, g) if a != b) if t and g else ''}"[:300]]))
    t = json_histograms(run([terrio, "info", "--json", "--hist", path]))
    g = json_histograms(run(["gdalinfo", "-json", "-hist", path], no_pam))
    out.append(("json", [] if t == g else [f"terrio {[x[:3] for x in t]} vs GDAL {[x[:3] for x in g]}"[:300]]))
    t = text_histograms(run([terrio, "info", "--stats", "--hist", path]))
    g = text_histograms(run(["gdalinfo", "-stats", "-hist", path], no_pam))
    out.append(("stats", [] if t == g else [f"terrio {[x[0] for x in t]} vs GDAL {[x[0] for x in g]}"[:300]]))
    # GDAL saves the histograms it computes in the .aux.xml sidecar, and later reads them back.
    g = text_histograms(run(["gdalinfo", "-hist", path]))
    t = text_histograms(run([terrio, "info", "--hist", path]))
    out.append(("stored", [] if t == g else [f"terrio {[x[0] for x in t]} vs GDAL {[x[0] for x in g]}"[:300]]))
    return out


def main():
    """Runs the comparisons."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data", help="the test data directory")
    ap.add_argument("--terrio", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
    ap.add_argument("--json", action="store_true", help="print JSON")
    a = ap.parse_args()
    terrio = os.path.abspath(a.terrio)
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio_hist_") as tmp:
        cases = []
        for name in FILES:
            if os.path.exists(os.path.join(a.data, name)):
                d = os.path.join(tmp, name + ".d")
                os.makedirs(d)
                cases.append((name, copy_dataset(a.data, name, d)))
        src = os.path.join(a.data, "srtm_sub.tif")
        if os.path.exists(src):
            for name, t in DERIVED:
                d = os.path.join(tmp, name + ".d")
                os.makedirs(d)
                p = os.path.join(d, name)
                run(["gdal_translate", "-q", "-ot", t, "-scale", "370", "2843", "-100", "60000" if t != "Int8" else "120", src, p])
                cases.append((name, p))
        for name, path in cases:
            try:
                diffs = compare(path, terrio)
            except RuntimeError as e:
                diffs = [("error", [str(e)])]
            for case, d in diffs:
                results.append({"name": f"{name} {case}", "file": name, "case": case, "differences": d})
                if not a.json:
                    print(f"{'OK  ' if not d else 'DIFF'}  {name} {case}" + (f": {'; '.join(d)}" if d else ""), flush=True)
    if a.json:
        print(json.dumps(results, indent=2))
    sys.exit(1 if any(r["differences"] for r in results) else 0)


if __name__ == "__main__":
    main()
