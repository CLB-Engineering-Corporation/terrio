"""Native interrupted Goode homolosine across the public Python surface."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/igh_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda case: case["name"])
def test_igh_oracle(case):
    samples = case["samples"]
    for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
        actual = np.column_stack(
            terrio.transform_points(
                [s[0] for s in samples], [s[1] for s in samples], case["source"], target
            )
        )
        assert np.all(np.abs(actual - [s[2] for s in samples]) * case["metres_per_unit"] <= 0.001)
    reversible = [sample for sample in samples if sample[3] is not None]
    actual = np.column_stack(
        terrio.transform_points(
            [s[2][0] for s in reversible],
            [s[2][1] for s in reversible],
            case["target"],
            case["source"],
        )
    )
    expected = np.asarray([s[3] for s in reversible])
    delta = actual - expected
    delta[:, 0] = ((delta[:, 0] + 180) % 360 - 180) * np.cos(np.deg2rad(expected[:, 1]))
    assert np.all(np.abs(delta) <= 1e-8)
    refused = [s[2] for s in samples if s[3] is None] + [
        p for p, expected in case["inverse_samples"] if expected is None
    ]
    for point in refused:
        with pytest.raises(terrio.CRSError):
            terrio.transform_points([point[0]], [point[1]], case["target"], case["source"])
