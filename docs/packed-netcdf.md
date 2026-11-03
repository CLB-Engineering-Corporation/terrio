# Packed NetCDF-4 snapshots

`multidim::Dataset::write_netcdf(path, &NetcdfWriteOptions { deflate: Some(4) })`
writes a new NetCDF-4 file through the existing HDF5 writer. Arrays with explicit
chunks use shuffle and deflate; contiguous coordinates and scalars remain contiguous.
Levels 0 through 9 are accepted. A destination is never overwritten. The corresponding
Python calls are `dataset.write(path, deflate=4)` and `dataset.to_bytes(deflate=4)`.
The CLI/MCP snapshot converter accepts `terrio mdconvert source.nc target.nc
--deflate 4`. JavaScript accepts `dataset.toBytes(4)`; omitting the level retains
the previous uncompressed behavior on every surface.

`Array::set_typed_attribute` and `Group::set_typed_attribute` attach native numeric
types to scalar or vector attributes. For CF-packed uint16 depths, set `scale_factor`
and `add_offset` as Float32 and `_FillValue` as 65535. Array bytes remain packed;
terrio never applies scale or offset during a multidimensional read or write.

A ras2fim depth stack uses dimensions `[flow,y,x]`, uint16 `depth`, chunks
`[1,ny,nx]`, ascending Float64 `flow`, Float64 cell-centre `x`/`y`, and a scalar
Int32 `spatial_ref`. Its GeoTransform and WKT are ordinary string attributes.
Global provenance and optional uint8 design-range flags are retained by the
generic dataset model. The application owns those conventions and units.

`cargo run -p terrio --example repack_netcdf -- source.nc destination.nc`
demonstrates raw-value preservation. HDF5 superblock/object layout, internal
dimension-scale records, `_NCProperties`, compression streams and file size differ
from netCDF-C output. Consumer equality concerns public attributes, native types,
dimensions, coordinates and packed values, rather than physical file hashes.

## Consumer comparison, 2026-11-01

A delivered FIM Commander median stack was opened read-only and rewritten with
terrio. NetCDF4 compared all 36,029,601 packed depth samples, 37 discharges,
771 y and 1,263 x coordinates, variable types/dimensions and every public
attribute with exact equality. Chunks remained `[1,771,1263]`, with shuffle and
deflate level 4. The unchanged vendored ras2fim-viewer-wasm `netcdf.js` and
h5wasm reader decoded identical depth arrays, discharges, coordinates, bounds,
packing and identity metadata. This is consumer software evidence, not
hydraulic qualification or a browser rendering/performance measurement.

| File | Bytes | SHA-256 |
|---|---:|---|
| netCDF-C reference | 4,773,446 | `ee6417e3048f7c49a7c369656d3dfad89c5b5c12d2bd0dfb5207c7b979e9c1c3` |
| terrio rewrite | 4,741,844 | `2ba2deb3a6ac89804c695ba8346e614d3af4e4183d9d12696b81e2016f671e9a` |

The viewer source SHA-256 was
`92d372441f98c45ef1eac0db15bceb060071fe569ae6bd158c825a23685814e4`.
Its decoded Uint16 depth buffer SHA-256 was
`d04f6547c5e25f4145d4f147000b4f4ae6a659832d36f16e9e709f088104ecf9`
for both files. The standalone `repack_netcdf` example reproduces the rewrite;
FIM Commander's measurement report identifies the immutable source unit.
