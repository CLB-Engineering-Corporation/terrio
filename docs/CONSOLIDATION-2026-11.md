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
the disk): 2,832 passed, 0 failed, 43 ignored. That first validation covered the Rust crates
only: pytest, the wheel and packaging checks, Linux CI and `cargo clippy` were not part of it, and
CI on `main` then failed in those (see "CI failures after the consolidation").

The first run had 2 failures. `open_sniffs_geoparquet_without_its_extension` came from the
branch and assumed its dropped `PAR1` sniff; it now turns on `TERRIO_WBVECTOR_FALLBACK`, as
main's detection requires. `another_origin_gets_no_header_a_document_supplies` failed on a
Windows socket error (10055, no buffer space) while the machine was heavily loaded, and passed
on the rerun without a change.

## CI failures after the consolidation

The first CI runs on the consolidated `main` failed in pytest and packaging. Only the GIL rule and
the licence list came from the consolidation itself. The overview and metadata failures were
already on the old `main`: "CI (macOS and Windows)" failed on 72133fb (run 41665704880) with them,
and "CI" had last passed on d1e1484, before the upstream merge in f88347f.

- **COG overviews differed from GDAL's (a policy conflict).** f88347f made `SOURCE` (`average` from
  the source blocks, ceil-sized levels) the default `COG_OVERVIEW_POLICY` and moved upstream's
  comparisons with GDAL to an explicit `GDAL_3_13`; `test_convert_alpha_cog.py`,
  `test_png_colorinterp.py` and `test_png_palette.py` kept comparing the default with GDAL. None of
  the branches suspected at first (the default-domain report, sidecar overviews, streamed blend)
  touches the overview pixels. Resolution: `SOURCE` stays the fork's default, as chosen in f88347f,
  and those three test files now name `GDAL_3_13`, as upstream's other comparisons already do.
- **Metadata domains.** An early return in `gdal_metadata_xml`'s `item` (from 70ef191, kept through
  f88347f) wrote an item with a domain without its `sample`, so a band's domain was read back as the
  dataset's. Not the effective-options resolution of `plan_with`. The early return is gone; the
  general path writes `sample`, `role` and `domain`.
- **GIL rule.** `python/src/pixel.rs` (the path-to-path `blend`, eb69ec3) and
  `python/src/geoparquet.rs` (`split_by_column`, be35cca) called `py.detach` directly. They now use
  `detach_result`, so a panic there is an `InternalError`. The HECRAS archive bindings were not
  involved.
- **Licence list.** 2019f8f regenerated the list on Windows with `core.autocrlf=true`. Git stripped
  the CRLF line endings of the licence texts that have them (939 lines), and terrio's own `LICENSE`,
  read with CRLF from the working tree, moved to another place in the list. The list is regenerated with
  cargo-about 0.9.2 from an LF checkout, as CI runs it, and `.gitattributes` keeps Git from changing
  its line endings.
- **Docs.** `docs.yml` downloads the Pyodide wheel of the latest GitHub release and the fork has no
  releases ("release not found"). Not a code failure, and nothing was changed for it: the workflow
  stays red on the fork until a release with that wheel exists.
- **macOS.** `test_overviews_are_gdals[3001x2999x1-uint16-nd0-AVERAGE]`,
  `test_masked_overviews_are_gdals[mask-f32-BILINEAR]` and
  `test_masked_build_overviews_are_gdals[mask-rgb-AVERAGE]` differ from GDAL by rounding in a few
  pixels on macOS only. `BIT_EXACT_ON_LINUX` in `python/tests/conftest.py` lists these tests under
  their old file, `test_cog_overviews.py`; f88347f moved them to `test_cog_gdal_policy.py`, so the
  entry no longer matches and they became required off Linux. They were left as they are: registering
  the new names marks them xfail off Linux, which is a decision for the repository's owner.

Tests changed: the three COG comparison files above name `GDAL_3_13`; their GDAL expectations are unchanged. Two test servers now count bytes before sending them.

Two test servers counted what they sent after writing it, so the client could return first:
`test_palette_destination_reuses_the_remote_source` and
`test_network_stats_report_gdals_counters_per_host` failed now and then (`assert 0 == 1794`,
`assert 0 > 0`). They now count first; their assertions are unchanged.

## Upstream migration: C dependencies

The fork had no upstream-migration list in the repository (`ROADMAP.md` is upstream's roadmap;
the local `QUEUE.md` of the upstream workstream lists prepared upstream PRs, not dependencies), so
this section is that list. It records every C dependency, and every dependency upstream cannot take,
that must be rewritten in pure Rust with similar performance, or removed, before the fork's work goes
to opengeos/terrio. The owner allowed C in the fork on 2026-11-08; upstream's principle is pure Rust
by default, every default feature building for WebAssembly without a C compiler.

| # | Dependency | Builds | Before upstreaming |
|---|---|---|---|
| 1 | **TLS: `ring`** (C and assembly), through `ureq`'s `rustls` | `remote` (default), **every native build** since T26 (2026-11-09): the library, the command, the native wheels and the Docker image, on every architecture | The fork uses `ring`, as `terrio-clb` does: `rustls-graviola` 0.4 (graviola 0.4.2, Rust and assembly), which upstream uses on x86-64 and AArch64, **panics** on x86-64 CPUs without AVX2, ADX or BMI2 (its `verify_cpu_features` asserts `aes`, `pclmulqdq`, `bmi1`, `adx`, `avx` and `avx2` on the first cryptographic call; its README also requires `bmi2` and `ssse3`), and on AArch64 without `aes`, `sha2`, `pmull` and `neon`. Upstream still needs a pure-Rust provider with `ring`-like speed and no CPU-feature panic (for example graviola with a pure-Rust fallback chosen at run time when its features are missing) before this can go back. `remote` left `drivers`, so `default-features = false, features = ["drivers"]` (and `terrio-cli --no-default-features`) still builds without C, without remote inputs. CI fails if a default native build links graviola. |
| 2 | **Zstandard of GeoTIFF/COG: `zstd` 0.14 / `zstd-sys`** (the zstd C library) | `libzstd-geotiff`, **opt-in since 2026-11-10** (it was on by default in terrio, terrio-cli and the native Python wheels from the owner's decision of 2026-11-08); never in WebAssembly | The pure-Rust path (`zstd-rust`) was 1.1 to 1.44x slower to write and about 2x slower to decode than libzstd when this was decided (the owner's figures; measured in fork PR #35: 1.16x to 1.51x to write, and in PR #36: a whole COG read end to end, chunk decoding plus the rest, 1.29x to 1.33x). With the whole-frame decoder and the row match finder of `zstd-rust` (gpt-cmdr/zstd-rs `main`) a whole read takes 0.91x to 1.02x as long and a whole write 0.86x to 1.38x on one thread, 0.61x to 1.16x on eight (docs/testing.md, "Zstandard: pure Rust against the C library"). The owner made pure Rust the default again on 2026-11-10, so this is no longer in the way of upstream. The all-pure default is kept on `feat/pure-zstd-geotiff-default` (c913ad7); see ROADMAP.md, "Pure-Rust GeoTIFF/COG Zstandard by default". |
| 3 | **`zstd-rust` is a git dependency** (gpt-cmdr/zstd-rs at e716f73), not on crates.io | every build: it is `ruzstd` for every pure Zstandard path, terrio-crs's bundled tables included | The owner chose not to publish it, so it blocks upstream use: `cargo publish` refuses a dependency without a crates.io version, and upstream cannot depend on a git repository. Publish it, contribute its levels to `ruzstd`, or return to crates.io `ruzstd` first. It is also why `release.yml` publishes no crates. |
| 4 | **ZSTD datum grids: the `tiff` crate's `zstd` 0.13 / `zstd-sys`** | terrio-crs's opt-in `libzstd` (terrio-crs built on its own; terrio does not enable it, so terrio reads no ZSTD TIFF grids) | ZSTD-compressed TIFF datum grids still need libzstd. A pure-Rust path (terrio-crs decoding the grid chunks with `ruzstd`, as terrio does for GeoTIFF) is needed for them. PROJ's CDN grids are DEFLATE-compressed, so no default path needs it today. |
| 5 | `libzstd-ept`, `libzstd-geoparquet`, `libzstd` (`zstd-sys`) | opt-in | Opt-in C, which upstream's principle allows. They go with item 2 if `zstd-rust` reaches parity. |
| 6 | `parquet-zstd` (`parquet/zstd`: `zstd` 0.13 / `zstd-sys`) | opt-in | Opt-in C, allowed upstream; the default ZSTD GeoParquet path is pure Rust (#35). |
| 7 | `gdal` (`gdal-sys`: GDAL's C and C++ libraries, found with `pkg-config`) | opt-in | Upstream's own opt-in backend: nothing to rewrite. |
| 8 | `rusty_av1d`'s `cc` build dependency | `avif` (default) | Compiles no C or assembly with its assembly feature off, as upstream builds it: nothing to rewrite. |

Audit (`cargo tree -e normal,build`, 2026-11-08, updated 2026-11-09): the default builds of terrio,
terrio-cli and terrio-py have `zstd-sys` (item 2), `ring` (item 1, on every architecture since
2026-11-09; before, `graviola` and `rustls-graviola` on x86-64 and AArch64) and `cc` (items 1, 2
and 8), and no other C crate; `--all-features` adds `gdal-sys` and `pkg-config`. terrio-js has no C
dependency.

**`rustls-graviola` in the default build and the wheels (fixed 2026-11-09, T26).** Until then the
`remote` feature, a default feature, installed `rustls_graviola::default_provider()` in `tls_config`
(`crates/terrio/src/http.rs`) on x86-64 and AArch64 with no CPU check or fallback, so the default
terrio library, the `terrio` command, the native Python wheels and the Docker image panicked on
their first HTTPS request on an x86-64 CPU without AVX2, ADX or BMI2 (about pre-2013 Intel and
pre-2015 AMD CPUs, and virtual machines that hide those flags). They now use `ring`, which detects
the CPU's features at run time and has portable code for each operation. CI's "No default native
build links graviola" step checks terrio, terrio-cli and terrio-py on the eight release targets.
HTTPS range reads of a public COG were as fast as before (network-bound; T26 measurements in the
pull request). terrio-js and the Pyodide wheel build no TLS and are unchanged.

**Known CI failure, not fixed (2026-11-08).** On `python (macos-latest)`,
`test_cog_gdal_policy.py::test_overviews_are_gdals[3001x2999x1-uint16-nd0-AVERAGE]`,
`test_cog_gdal_policy.py::test_masked_overviews_are_gdals[mask-f32-BILINEAR]` and
`test_cog_gdal_policy.py::test_masked_build_overviews_are_gdals[mask-rgb-AVERAGE]` differ from GDAL
by rounding, on `main` and on every PR (see "CI failures after the consolidation"). They could be
registered in `BIT_EXACT_ON_LINUX` (`python/tests/conftest.py`) under their new file name
(`test_cog_gdal_policy.py::test_overviews_are_gdals`, `::test_masked_overviews_are_gdals`,
`::test_masked_build_overviews_are_gdals`), which marks them xfail off Linux. Left to the owner.
