# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""The tiled-write comparison reports why a terrio command failed."""

import importlib.util
import sys
import types
from pathlib import Path

import pytest

pytest.importorskip("osgeo.gdal")
# These tests exercise only the script's command runner and argument parsing, never terrio
# itself, so they run where the wheel is not installed yet (the real-data CI job runs
# scripts/tests before building it). The script imports terrio at module level.
if importlib.util.find_spec("terrio") is None:
    sys.modules["terrio"] = types.ModuleType("terrio")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare_tiled_writes_with_gdal as tiled  # noqa: E402


def test_a_failed_translate_reports_its_stderr():
    cmd = [sys.executable, "-c", "import sys; sys.stderr.write('unknown creation option FOO'); sys.exit(2)"]
    with pytest.raises(RuntimeError, match=r"status 2: unknown creation option FOO"):
        tiled.run_cli(cmd)


def test_the_comparison_takes_no_data_directory(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["compare_tiled_writes_with_gdal.py", "data"])
    with pytest.raises(SystemExit) as stopped:
        tiled.main()
    assert stopped.value.code == 2
