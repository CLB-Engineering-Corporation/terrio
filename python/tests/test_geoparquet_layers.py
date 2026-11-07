"""GeoParquet layout options: `write_geoparquet_layers` from Arrow data, and the GeoParquet layer
creation options of `Layer.write`, `Layer.to_bytes` and `VectorDataset.write` (several layers in
one file with a layer column, Zstandard, Hilbert order, the covering name). Without pyarrow, the
inputs are terrio's own Arrow exports and the files are read back with terrio."""

import json
import struct
from pathlib import Path

import pytest

import terrio

HECRAS = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "hecras"


def points(name, coords, value_field="depth"):
    """A layer of points with an `id` and a float field."""
    features = [
        {
            "type": "Feature",
            "properties": {"id": i, value_field: float(i) / 2},
            "geometry": {"type": "Point", "coordinates": list(c)},
        }
        for i, c in enumerate(coords)
    ]
    collection = {"type": "FeatureCollection", "features": features}
    return terrio.Layer.from_geojson(collection, name=name, crs="EPSG:32615")


def test_arrow_layers_share_one_file_with_a_layer_column(tmp_path):
    a = points("a", [(10, 10), (0, 0), (5, 5)])
    b = points("b", [(1, 2)], value_field="stage")
    out = tmp_path / "both.parquet"
    options = {
        "LAYER_COLUMN": "layer",
        "COMPRESSION": "ZSTD",
        "COMPRESSION_LEVEL": "3",
        "SPATIAL_ORDER": "HILBERT",
        "HILBERT_COLUMN": "hilbert_index",
        "COVERING_BBOX_NAME": "bbox",
    }
    report = terrio.write_geoparquet_layers(
        [("a", a), ("b", b)], out, options=options, identity=["id"]
    )
    assert report["rows"] == 4
    assert report["layers"] == 2
    assert report["row_groups"] == 2
    assert report["bbox"] == [0.0, 0.0, 10.0, 10.0]
    back = terrio.open_vector(str(out))["both"]
    columns = back.columns()
    assert columns["layer"] == ["a", "a", "a", "b"]
    # Hilbert order within each layer: the diagonal from the origin.
    assert columns["id"] == [1, 2, 0, 0]
    assert columns["stage"] == [None, None, None, 0.0]
    keys = columns["hilbert_index"]
    assert keys[:3] == sorted(keys[:3])
    # The CRS of the inputs' GeoArrow metadata.
    assert "32615" in (back.crs or "")


def test_layer_creation_options_of_vector_writes(tmp_path):
    ds = terrio.open_vector(str(HECRAS / "results_mesh_xs.h5"))
    out = tmp_path / "plan.parquet"
    # One GeoParquet file holds several layers only with a layer column.
    with pytest.raises(terrio.InvalidArgumentError):
        ds.write(str(out))
    ds.write(str(out), options={"LAYER_COLUMN": "layer", "SPATIAL_ORDER": "HILBERT"})
    back = terrio.open_vector(str(out))
    names = back[0].columns()["layer"]
    assert len(names) == sum(len(layer) for layer in ds.layers)
    assert list(dict.fromkeys(names)) == ds.layer_names
    # One layer, in memory, with the options.
    data = ds["mesh_cells"].to_bytes("cells.parquet", options={"COMPRESSION": "ZSTD"})
    assert data[:4] == b"PAR1"
    # Unknown and misplaced options are refused.
    with pytest.raises(terrio.InvalidArgumentError):
        ds["mesh_cells"].write(str(tmp_path / "x.parquet"), options={"NO_SUCH": "1"})
    with pytest.raises(terrio.InvalidArgumentError):
        ds["mesh_cells"].write(str(tmp_path / "x.geojson"), options={"COMPRESSION": "ZSTD"})
    with pytest.raises(terrio.UnsupportedError):
        ds["mesh_cells"].write(str(tmp_path / "x.parquet"), options={"SORT_BY_BBOX": "YES"})


def test_refusals_of_write_geoparquet_layers(tmp_path):
    a = points("a", [(0, 0)])
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_geoparquet_layers([("a", a), ("b", a)], tmp_path / "x.parquet")
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_geoparquet_layers(a, tmp_path / "x.parquet", options={"NO_SUCH": "1"})
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_geoparquet_layers(
            a, tmp_path / "x.parquet", options={"COMPRESSION": "SNAPPY", "COMPRESSION_LEVEL": "3"}
        )


def test_pyarrow_reads_the_file_and_types_are_kept(tmp_path):
    pa = pytest.importorskip("pyarrow")
    pq = pytest.importorskip("pyarrow.parquet")

    def wkb(x, y):
        return b"\x01\x01\x00\x00\x00" + struct.pack("<dd", x, y)

    table = pa.table(
        {
            "cell_id": pa.array([3, 1, 2], pa.int64()),
            "elevation": pa.array([1.5, None, 3.0], pa.float32()),
            "stations": pa.array([[1.0, 2.0], None, []], pa.list_(pa.float64())),
            "geometry": pa.array([wkb(9, 9), wkb(0, 0), wkb(4, 4)], pa.binary()),
            "layer": pa.array(["cells", "cells", "points"]),
        }
    )
    out = tmp_path / "t.parquet"
    report = terrio.write_geoparquet_layers(
        table,
        out,
        options={"LAYER_COLUMN": "layer", "COMPRESSION": "ZSTD", "STATISTICS": "CHUNK"},
        crs="EPSG:32615",
    )
    assert report["layers"] == 2
    got = pq.read_table(out)
    assert got.schema.field("elevation").type == pa.float32()
    assert got.schema.field("stations").type.value_type == pa.float64()
    assert got.column("layer").to_pylist() == ["cells", "cells", "points"]
    meta = pq.ParquetFile(out).metadata
    assert meta.row_group(0).column(0).compression == "ZSTD"
    geo = json.loads(meta.metadata[b"geo"])
    assert geo["columns"]["geometry"]["crs"] == "EPSG:32615"
    assert geo["columns"]["geometry"]["covering"]["bbox"]["xmin"] == ["geometry_bbox", "xmin"]
