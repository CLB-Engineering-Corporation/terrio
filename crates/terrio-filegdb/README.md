# terrio-filegdb

Pure-Rust reader and writer for Esri File Geodatabases (`.gdb`). No C dependencies; Windows and
Linux.

## Attribution

A port of the relevant parts of GDAL's **OpenFileGDB** driver
(<https://github.com/OSGeo/gdal/tree/master/ogr/ogrsf_frmts/openfilegdb>, MIT licence, Copyright
(c) Even Rouault and GDAL contributors), plus the format notes at
<https://github.com/rouault/dump_gdbtable/wiki/FGDB-Spec>. The XML item definitions the writer
produces follow what ArcGIS Pro 3.x writes, as read from geodatabases ArcGIS created.

Written by CLB Engineering Corporation. MIT licence.

terrio reads geodatabases with this crate, as the `OpenFileGDB` vector driver (its `filegdb`
feature); use the crate directly to write them or to keep true curves.

## API

```rust,ignore
use terrio_filegdb::{Geodatabase, RowFilter, Value};

let gdb = Geodatabase::open("data.gdb")?;
let t = gdb.open_table("Buildings")?;            // case-insensitive
let height = t.field_index("Height").unwrap();
// Only rows whose geometry envelope meets the bbox (fast: no vertex decoding).
for row in t.rows_filtered(RowFilter { columns: None, bbox: Some([xmin, ymin, xmax, ymax]) })? {
    let row = row?;
    let shape = row.shape();               // Option<&Shape>
    let v = row.values[height].as_f64();
}
```

* `Geodatabase::{open, table_names, all_table_names, has_table, open_table}`
* `Table::{name, fields, field_index, geometry_info, geometry_field_index, feature_count,
  max_object_id, read_all, rows, rows_filtered, spatial_index_grid, has_spatial_index_file}`
* `Shape { shape_type, parts, has_curves, curves }` with `area()`, `length()`, `envelope()`,
  `contains_point(x, y)` and `densify(step_deg)`. These are **exact for circular arcs** (true
  curves), computed as chord polygon +/- circular segments. `Shape::new(type, parts)` builds a
  straight shape.
* `GdbWriter::{create, add_coded_domain, create_table, insert, finish}` with `TableDef`,
  `FieldDef`, `GeometryDef`. `insert(table, shape: Option<&Shape>, values)` takes values parallel
  to `TableDef::fields`. OBJECTID, `Shape`, `Shape_Length` and `Shape_Area` are managed (0 for a
  null shape, as ArcGIS stores them). Circular-arc curves are written as ArcGIS writes them.

Coordinates decode bit-identically to ArcGIS and GDAL: `integer / scale + origin`.

## Verification

Geodatabases written by this crate, about 9,000 rows covering every field type, null shapes and
true curves, were read with ArcGIS Pro 3.6, GDAL 3.13 and QGIS 4.2. Every attribute and vertex
matched with no mismatches, and schema, CRS, aliases and coded-value domains were correct. ArcGIS
also passed Check Geometry, edits, Compact and spatial queries on them. Reading was checked
against arcpy the same way, on geodatabases of up to 1.16 million features.

## Limitations

* `Shape_Area` written by `GdbWriter`, and `Shape::area()`, agree with Esri's value to within 4
  ULP for small polygons and about 3e-13 relative for large dissolved ones, not bit for bit. Esri's
  floating-point summation order is unknown. arcpy's `SHAPE@AREA` is recomputed from the geometry,
  which is written bit-identically.
* The writer creates the `.gdbindexes` registry (`FDO_OBJECTID`, `FDO_Shape`) and a `.spx` spatial
  index for every feature class (ported from GDAL's `filegdbindex_write.cpp`). No attribute indexes
  (`.atx`) are written.
* Writing curves supports circular arcs; Bezier and elliptic segments are stored but measured by
  densification. Z/M writing and multipatch writing are not supported. Raster fields are skipped.
* `FieldInfo::alias` is the alias of the `.gdbtable` field descriptor (what GDAL and QGIS show).
  ArcGIS shows the `AliasName` of the GDB_Items XML instead. The writer writes the same alias to
  both.
* Tables without a `.gdbtablx`, and version-4 (64-bit OID) tables with an unrecognised block map,
  return `Error::Unsupported`.
