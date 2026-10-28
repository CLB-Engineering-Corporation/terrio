# HEC-RAS fixtures

Synthetic HEC-RAS-shaped HDF5 files for the HECRAS driver's tests. They contain no model data.
The `coverage_*` files are copied unchanged from the engine repository,
[gpt-cmdr/ras-commander-fast-hdf](https://github.com/gpt-cmdr/ras-commander-fast-hdf) (MIT,
Copyright (c) 2024 CLB Engineering Corporation; see NOTICE), `tests/fixtures/coverage/` at
`d6d52f41cb0ef0d4170f60db25d28e01dd543ecd` (release 0.2.0; the same bytes as at the earlier
`a7243bfa36b2fb7d2d54c1a49ea2906e68d6ef3d`). Their generator and full manifest are listed in that
directory's `coverage_manifest.json`.

| File | SHA-256 | Contents |
|---|---|---|
| `coverage_mesh.h5` | `ff8172ccd3ff76e643416faa474110e6ba8bf8ff218333e8423dbe8591491117` | 19 meshes: `Good` (two drawable cells and a one-face cell), precision and width variants, and broken topologies |
| `coverage_mesh_nogroups.h5` | `4ed5ad7c89e14ff367a4a0ac34d2ea85c5b78913d651d62a49fe1eddedef84fd` | Legacy names-only layout (the engine refuses it) |
| `coverage_mesh_nogeometry.h5` | `3117dbec3e3c323ffbec8ea95728a55b3e592d5f792ab6fa15151902e0220485` | No `Geometry/2D Flow Areas` group |
| `results_mesh_xs.h5` | `a1b749a22fb2836aa4b5003eb2a1ccc3de75da5306be840348567095af067118` | Written by `scripts/make_hecras_fixture.py` from `coverage_mesh.h5`: the `Good` mesh with made-up static arrays, summary, time series and three cross sections with summary and unsteady results, two boundary-condition lines with series, two reference points and one reference line with results, and one river junction, in the stored HEC-RAS 6.x/7.0 layout. No model data |

Real HEC-RAS models are not committed. `tests/hecras_corpus.rs` reads the files of a corpus
directory (`TERRIO_HECRAS_TEST_DATA`) and compares them with the JSON that
`scripts/hecras_oracle.py` writes with h5py; `scripts/compare_hecras_with_h5py.py` runs both on
the HEC-RAS 7.0.1 files of a directory.
