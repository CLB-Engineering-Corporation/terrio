# Architecture

```text
crates/terrio        the library
  dataset.rs         open, read and write; format detection; fixes applied after reading
  formats.rs         the driver registry, with GDAL's short names
  drivers/           terrio's own raster drivers; drivers/vector/ its vector drivers
  geotiff.rs, cog.rs GeoTIFF tags and decoding, lazy window and overview reads
  block_cache.rs     process-wide LRU cache of decoded GeoTIFF tiles and strips
  tiles/             PMTiles, MBTiles and MVT: archives, tile pyramids, vector tile clipping
  vfs.rs             locations: paths, URLs, s3://, gs://, az://, zip archives, /vsi prefixes
  http.rs, s3.rs     range requests with a block cache; AWS Signature Version 4
  crs.rs             terrio::crs, over crates/terrio-crs
  multidim/          native groups, dimensions and arrays
  pointcloud/        LAS, LAZ, COPC, EPT, PLY, E57 and filters
  vsi/               terrio vsi file management
  ops/               translate, warp, mosaic, vrt, vector filters and SQL, rasterize, polygonize
  memory.rs          datasets from and to bytes
  info.rs            summaries
crates/terrio-crs    standalone coordinate systems: parsing, identification, transformation
crates/terrio-cli    the terrio command and MCP server
python/              Python bindings (PyO3, NumPy)
js/                  WebAssembly bindings (wasm-bindgen)
scripts/             test data, comparisons with GDAL, benchmarks
```

## Data model

Rasters are held as `wbraster::Raster`: typed pixel buffers, band after band and row after row,
with the georeferencing and CRS. `RasterDataset` adds the driver and whether nodata is declared.
GeoTIFFs, tile archives (PMTiles, MBTiles), JPEG 2000, Zarr, GRIB and raw-layout rasters can also
be opened lazily, to read windows and
overviews without loading the image.

Vector datasets are `wbvector::Layer`s: features with OGC simple-feature geometries (with Z and
M), typed attributes and a schema.

## Building on whitebox_next_gen

The `wbraster`, `wbvector` and `wbprojection` crates supply the data model, many codecs and a
fallback for projections terrio does not compute itself. Testing them against GDAL found codecs that decoded wrong pixels or lost
georeferencing, so terrio has its own drivers for GeoTIFF, JPEG 2000, Zarr, netCDF, HFA,
GeoPackage (on its own SQLite file reader and writer), the Esri formats, FlatGeobuf, CSV,
GeoJSON, GeoParquet, GML, GPX, KML, TopoJSON, MapInfo, OSM and shapefile writing. Upstream bugs
are listed in the pull requests that replaced them.

## Parallelism

Native builds spread the heavy work over all cores with rayon: decoding and encoding GeoTIFF
tiles and strips, building COG overviews, warping and resampling (by output row), rasterizing (by
bands of rows), writing Zarr chunks, and decoding and encoding vector features. Each piece of
work is computed on its own and the results are combined in input order, so outputs are the same
bytes as a single-threaded run gives. WebAssembly builds run the same code sequentially.

All of it runs on one thread pool, terrio's own (`crates/terrio/src/par.rs`), never on rayon's
global pool. A `fork()`ed child, as Python's `multiprocessing` starts on Linux, has only the
thread that forked: a pool started before the fork has no threads there, and work given to it
would wait forever. terrio's pool is per process, so the first parallel call in a child starts a
new one. A fork also waits for terrio's background work in progress to finish. New parallel code
uses `par`'s functions: `par::map_range`, `par::for_each_owned` and the like for data parallelism,
`par::spawn` for work in the background, and `par::install` around rayon code they cannot express
or a dependency that uses rayon by itself. `clippy.toml` disallows rayon's global-pool functions
and parallel iterators everywhere else.

## Pure Rust

Every driver builds without C dependencies, and CI builds them without a C compiler: the
WebAssembly builds always, and natively with `default-features = false, features = ["drivers"]`
(`terrio-cli --no-default-features`). One exception is on by default in the CLB fork: GeoTIFF/COG
Zstandard uses the zstd C library natively (`libzstd-geotiff`, a default feature of terrio,
terrio-cli and the Python package), because the pure-Rust path is not yet as fast (1.1 to 1.5 times
slower to write; a whole ZSTD COG read takes 1.3 times as long). The pure-Rust GeoTIFF default is kept on the branch
`feat/pure-zstd-geotiff-default` until it reaches parity (see the ROADMAP). Three choices keep the
rest pure Rust:

- Zstandard is decoded and written with `ruzstd` but for that exception: Zarr, EPT and GeoParquet's
  pages, and GeoTIFF/COG in WebAssembly and in builds without `libzstd-geotiff`. terrio uses
  `zstd-rust`, CLB's fork of `ruzstd` with the `Default`, `Better` and `Best` levels, from git
  (gpt-cmdr/zstd-rs; not on crates.io) under the `ruzstd` name. zstd's levels map to its four:
  2 and below `Fastest`, 0 and 3 to 5 `Default`, 6 to 9 `Better`, 10 and up `Best`. Its output
  decodes to the same bytes as the zstd C library's and each reads the other's. The opt-in
  `libzstd-ept` and `libzstd-geoparquet` features (or `libzstd`, all three) use the C library
  natively for EPT and GeoParquet too.
- HTTPS uses `rustls` with `ring` on every architecture (CLB fork). `ring` is C and assembly, so
  the `remote` feature needs a C compiler; it detects the CPU's features at run time and runs on
  any CPU. Upstream uses `rustls-graviola` on x86-64 and AArch64, which panics on CPUs without
  AVX2, ADX or BMI2; see `docs/CONSOLIDATION-2026-11.md`. `remote` is a default feature but not
  part of `drivers`, so a build with `default-features = false, features = ["drivers"]` still
  needs no C compiler, and has no remote inputs.
- GeoParquet is written with Snappy or Zstandard. The `parquet` crate's Zstandard is the C
  library, so terrio has the crate write ZSTD pages uncompressed and compresses each page itself
  (`drivers/vector/geoparquet_zstd.rs`): the pages, encodings, dictionaries, statistics and page
  index are the crate's, and only the compressed bytes differ from the crate's own codec. The
  opt-in `parquet-zstd` feature uses the crate's codec (the C library) instead. Reading also takes
  gzip, LZ4 and Brotli.

JPEG 2000 uses a Rust port of OpenJPEG, in WebAssembly too: terrio supplies the C allocator calls
the port makes. JPEG is decoded by terrio's own decoder, which reproduces libjpeg's arithmetic;
LERC, JPEG XL and LZMA by pure-Rust crates.
