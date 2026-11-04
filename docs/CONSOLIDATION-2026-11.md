# Branch consolidation, November 2026

Every feature branch of the CLB fork (CLB-Engineering-Corporation/terrio) was
combined into `main` on 2026-11-03, so downstream CLB projects can depend on the
fork's `main`. Nothing was pushed to opengeos/terrio. The previous `main`
(72133fb) is kept as `backup/main-before-consolidation`. No branch was deleted.

The consolidation is a chain of ordinary merges on top of the old `main`. Where a
branch's early commits had already reached `main` in another form (squash-merged
upstream, or rebased), the merge was computed against the commit the two lines
share (`git merge-tree --merge-base=<commit>`) so that only the branch's own later
work applied, then recorded as a normal two-parent merge.

## Merged

| Branch | Tip | Merge | Notes |
|---|---|---|---|
| feat/hdf5-reader-0.10 | 1635d2e4 | e7dc097 | Already in main (hdf5-reader/netcdf-reader 0.10.1 came with upstream #376); kept main's pins. |
| feat/geoparquet-archive-profile | cb0b0a79 | be35cca | GeoParquet from Arrow layers (archive profile), Arrow/Parquet 59. |
| feat/hecras-arrow-archive (contains feat/hecras-hdf-layers 005eb43d) | bd07318e | 8ae3414 | HECRAS driver on fast-hdf's Arrow catalog, HDF to archive GeoParquet. Merge base a01fff5. |
| feat/cog-report-default-domain | 7a79cc85 | 7d9a0ef | Area-matched report in the default metadata domain; NaN-valid area matching. |
| fix/cog-validate-followups | 4fe27d02 | 63abcc2 | Both commits already present in main's form (NOTICE credit, NUL escape). |
| upstream/cog-sidecar-overviews | 7c00150b | 51477d3 | Only 7c00150b (every .ovr image is an overview, as GDAL) was new. Merge base 09e75a3d (#362). |
| upstream/blend-streaming | 66795234 | eb69ec3 | `blend` path to path in bounded memory. |
| feat/lazy-point-sampling | c83fd1e1 | 0c7ccc3 | `sample --lazy` with status fields. |
| feat/lerc-encoder-option (PR #26) | 62f22e41 | 19705b1 | `LERC_ENCODER`; lerc 0.5.2 from crates.io replaces the git patch. Supersedes upstream/lerc-encoder-option. |
| feat/multidim-packed-netcdf (PR #31) | 76e20d75 | a9a59c9 | Deflated multidimensional NetCDF snapshots, typed attributes. |
| feat/scientific-browser-conformance-b | 576bddc8 | 04395a8 | 12f93c4 plus its merge of upstream #508/#509. Merge base bff7bd9. |
| feat/fork-scientific-contributions | 7f09be44 | 968a677 | Fuller fork versions of main's WASM/multidim commits; see conflicts. |
| perf/owned-multidim-c | c6e89278 | b5700c3 | Patch-identical to 7f09be44; recorded with `-s ours`. |

Also committed: 420229b (Cargo.lock for Arrow/Parquet 59) and this report.

## Not merged (already in main, superseded or obsolete)

Already ancestors of the old main (nothing to do): feat/bounded-warp,
feat/calc-streaming, feat/clb-semantic-reconciliation-resume, feat/cog-area-ratios,
feat/cog-overview-factors, feat/cog-supplied-overviews, feat/cog-validate,
feat/mosaic-exact-strips, feat/mosaic-windowed-reads, feat/shaded-relief,
feat/streaming-raster-tiles, feat/windowed-dem, fix/edge-tile-nodata-padding.

| Branch | Reason |
|---|---|
| feat/selective-scientific-wasm | Tree-identical to main (upstream #503). |
| fix/windows-tile-index-separators | Tree-identical to main (PR #30, upstream e200556). |
| feat/tile-overview-resampling-trial | Its commit is in main (same patch). |
| feat/wasm-viewer-contributions | Its commit is in main (9c644f4, same change). |
| feat/streaming-calc (PR #21) | Superseded by feat/calc-streaming (#25), in main. |
| feat/points-hexbin, fix/navd88-feet-geoid, fix/points-reproject-z, fix/hdf5-reader-v1-compounds, fix/dem-streaming-followups | Squash-merged upstream (#331, #332, #336, #98, #369) at exactly these tips; in main. |
| upstream/hecras-driver, upstream/dem-streaming, upstream/raster-tiles-streaming, upstream/cog-validate | Squash-merged upstream (#376, #363, #368, #362) at exactly these tips; in main. |
| upstream/calc-streaming, upstream/mosaic-windowed-reads, upstream/warp-mosaic-bounded, upstream/cog-options-overviews, upstream/cog-area-ratios, upstream/cog-overview-factors, upstream/edge-tile-nodata-padding, upstream/dem-shaded-relief | Upstream-PR variants of fork branches already merged into main (#24, #25, #13, #9, #18, #19, #22, #12). |
| upstream/lerc-encoder-option | Same change as feat/lerc-encoder-option without the lerc patch; that branch is merged. |
| netcdf4-hdf5 | Upstream's own branch, squash-merged as #55; the extra commit only regenerated a conformance report main has since regenerated. |
| rio-compat | Upstream's own branch, squash-merged as #59; its later review-fix commit (150fe3a4) was re-landed differently upstream (driver inference, VRT block cache). |
| release-dry-run | Upstream's own branch, merged as #66; its last commit (drop rust-cache from the crates dry run) predates main's rewritten workflow. Not applied. |
| python-wheels-platforms | Superseded: main's release-build.yml already builds musl aarch64, Windows ARM64 and Pyodide wheels. |

## Conflicts and resolutions

- **GeoParquet zstd (C dependency).** The branch enabled `parquet/zstd` in the default
  `parquet` feature. Main forbids C in the default and WebAssembly builds (#444; CI checks
  it) and reads Zstandard pages itself. Resolution: parquet 59 with main's pure-Rust codec
  set, and a new opt-in feature `parquet-zstd` for ZSTD GeoParquet writing. Without it,
  ZSTD writing is refused with an error. The native Python wheel enables it; the Pyodide
  wheel does not. The archive tests require it.
- **GeoParquet detection.** The branch added an unconditional `PAR1` sniff in
  `open_local`. Main already detects GeoParquet by magic behind
  `TERRIO_WBVECTOR_FALLBACK` and the driver allow-list, and its test expects that. The
  branch's sniff was dropped.
- **HECRAS, two implementations.** Main had the driver as reviewed upstream (#376, five
  review rounds: byte budgets, point rows, field checks). The branch moved the same driver
  onto fast-hdf 0.3.0's Arrow catalog and added the archive writer. Both were kept:
  `family_layer` reads the catalog layer with upstream's budgets
  (`LayerOptions::with_budgets`, `crs_within`). fast-hdf 0.3.0 now comes from crates.io
  (the git tag patch is gone). The Python crate gained a default `hecras` feature for the
  archive bindings. Docs, NOTICE, REUSE.toml and the fixture README keep main's layout
  and add the arrow_layers.h5 fixture.
- **COG report.** Main moved `plan_with` onto an effective-options copy with metadata
  domains. The report's default-domain items go into `effective.metadata`, and its
  OVERVIEW_REPORT items into `effective.domains`.
- **Streamed RGBA writes (a bug the combination exposed).** The blend branch's test
  requires a streamed COG to be byte-identical to the whole write. Main's `StripWriter`
  did not derive `PHOTOMETRIC=RGB`/`ALPHA=YES` from band colour interpretations as
  `write_raster_dataset` does, so a streamed RGBA output was MINISBLACK plus extra
  samples. `StripWriter::create_masked` now applies the same `color_options`.
- **LERC.** lerc-writer 0.5.2 on crates.io has `EncoderProfile`, so the git patch was
  replaced with a 0.5.2 requirement.
- **Multidim owned bytes.** Main's public `multidim::open_bytes_owned` skipped the driver
  allow-list. The fork branch's check and regression were kept. Main's reviewed docs and
  WASM runners were kept. The branch's repeated-initialization lifecycle benchmark
  (`js/tests/scientific-lifecycle.mjs`), which main's runner no longer calls, was left
  out, as was a provenance note main now carries inline.
- **Docs and changelog.** Where main had rewritten the text, the branch's additions were
  re-applied onto main's wording: CHANGELOG entries under Unreleased, docs/cli.md,
  docs/testing.md and scripts/conformance.py.
- `LICENSE-THIRD-PARTY.md` was regenerated with cargo-about 0.9.2 for Arrow/Parquet 59,
  fast-hdf 0.3.0 and lerc 0.5.2 (see below).

## Validation (Windows 11, Rust 1.99.1 x86_64-pc-windows-gnu)

`cargo test --workspace --no-fail-fast` (debug info and incremental compilation off, to fit
the disk): 2,832 passed, 0 failed, 43 ignored. Linux CI and `cargo clippy` were not run for
this consolidation.

The first run had 2 failures. `open_sniffs_geoparquet_without_its_extension` came from the
branch and assumed its dropped `PAR1` sniff; it now turns on `TERRIO_WBVECTOR_FALLBACK`, as
main's detection requires. `another_origin_gets_no_header_a_document_supplies` failed on a
Windows socket error (10055, no buffer space) while the machine was heavily loaded, and passed
on the rerun without a change.
