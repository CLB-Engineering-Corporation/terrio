use super::*;
use arrow_array::{BooleanArray, Float32Array, Int64Array, ListArray, types::Float64Type};
use parquet::file::reader::{FileReader, SerializedFileReader};

fn point(x: f64, y: f64) -> Vec<u8> {
    [&[1u8, 1, 0, 0, 0][..], &x.to_le_bytes(), &y.to_le_bytes()].concat()
}

/// A big-endian square polygon with a hole, `size` wide at `(x, y)`.
fn square(x: f64, y: f64, size: f64) -> Vec<u8> {
    let ring = |pts: &[(f64, f64)]| {
        let mut out = (pts.len() as u32).to_be_bytes().to_vec();
        for (a, b) in pts {
            out.extend(a.to_be_bytes());
            out.extend(b.to_be_bytes());
        }
        out
    };
    let outer = [(x, y), (x + size, y), (x + size, y + size), (x, y + size), (x, y)];
    let c = (x + size / 2.0, y + size / 2.0);
    let hole = [c, (c.0 + 0.1, c.1), (c.0, c.1 + 0.1), c];
    [&[0u8][..], &3u32.to_be_bytes(), &2u32.to_be_bytes(), &ring(&outer), &ring(&hole)].concat()
}

fn empty_polygon() -> Vec<u8> {
    [&[1u8][..], &3u32.to_le_bytes(), &0u32.to_le_bytes()].concat()
}

fn crs_field() -> Field {
    Field::new("geometry", DataType::Binary, true).with_metadata(HashMap::from([
        ("ARROW:extension:name".to_string(), "geoarrow.wkb".to_string()),
        (
            "ARROW:extension:metadata".to_string(),
            r#"{"crs": {"id": {"authority": "EPSG", "code": 26918}}, "crs_type": "projjson"}"#.to_string(),
        ),
    ]))
}

/// A layer of `mesh_name`, `cell_id`, an extra column and geometries.
fn layer(name: &str, extra: (&str, ArrayRef), geometries: Vec<Option<Vec<u8>>>) -> MemoryLayer {
    let n = geometries.len();
    let schema = Arc::new(Schema::new(vec![
        Field::new("mesh_name", DataType::Utf8, true),
        Field::new("cell_id", DataType::Int64, true),
        Field::new(extra.0, extra.1.data_type().clone(), true),
        crs_field(),
    ]));
    let wkb: BinaryArray = geometries.iter().map(|g| g.as_deref()).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n("m", n))),
            Arc::new(Int64Array::from_iter_values(0..n as i64)),
            extra.1,
            Arc::new(wkb),
        ],
    )
    .unwrap();
    ArrowLayer::new(Some(name.into()), schema, vec![Ok(batch)]).with_identity(["mesh_name", "cell_id"])
}

/// The schema, rows, `geo` metadata and row-group sizes of a file, read as terrio's GeoParquet
/// reader reads it (Zstandard pages with `crate::zstandard`, the `parquet` crate's codec unused).
fn read(bytes: &[u8]) -> (SchemaRef, RecordBatch, Value, Vec<usize>) {
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
    let bytes = bytes::Bytes::copy_from_slice(bytes);
    let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::new()).unwrap();
    let schema = meta.schema().clone();
    let groups: Vec<usize> = (0..meta.metadata().num_row_groups()).collect();
    let reader =
        crate::drivers::vector::geoparquet_zstd::reader(bytes.clone(), meta.metadata(), &schema, ProjectionMask::all(), groups, None, 1024)
            .unwrap();
    let batches: Vec<RecordBatch> = reader.map(|b| b.unwrap()).collect();
    let batch = arrow_select::concat::concat_batches(&schema, &batches).unwrap();
    let reader = SerializedFileReader::new(bytes).unwrap();
    let meta = reader.metadata();
    let geo = meta.file_metadata().key_value_metadata().unwrap().iter().find(|kv| kv.key == "geo").unwrap().value.clone().unwrap();
    let groups = meta.row_groups().iter().map(|g| g.num_rows() as usize).collect();
    (schema, batch, serde_json::from_str(&geo).unwrap(), groups)
}

fn ids(batch: &RecordBatch) -> Vec<i64> {
    batch.column_by_name("cell_id").unwrap().as_primitive::<arrow_array::types::Int64Type>().values().to_vec()
}

fn strings(batch: &RecordBatch, name: &str) -> Vec<String> {
    batch.column_by_name(name).unwrap().as_string::<i32>().iter().map(|v| v.unwrap().to_string()).collect()
}

#[test]
fn hilbert_matches_duckdb_spatial() {
    // DuckDB spatial: ST_Hilbert(x, y, {'min_x':0,'min_y':0,'max_x':10,'max_y':10}::BOX_2D)
    let extent = [0., 0., 10., 10.];
    for (x, y, expected) in [
        (0., 0., 0u32),
        (10., 10., 2863311530),
        (5., 5., 715827882),
        (1.5, 2.5, 157903209),
        (9.99, 0.01, 4294959096),
        (3.3, 7.7, 1629721655),
        (0., 10., 1431655765),
        (10., 0., 4294967295),
        (2.5, 2.5, 178956970),
    ] {
        assert_eq!(hilbert_index(x, y, extent), expected, "({x}, {y})");
    }
    // A degenerate axis is cell 0; out-of-extent values saturate.
    assert_eq!(hilbert_index(3., 3., [3., 3., 3., 3.]), 0);
    // Outside the extent, cells saturate to u32 and keep their low 16 bits, as in DuckDB.
    assert_eq!(hilbert_index(-5., 20., extent), hilbert_encode_16(0, 131070 & 0xFFFF));
}

#[test]
fn hilbert_permutation_is_stable_with_empty_rows_last() {
    let b = |x: f64, y: f64| Some([x, y, x, y]);
    let keys = hilbert_keys(&[None, b(10., 0.), b(0., 0.), b(0., 0.), None, b(5., 5.)]);
    assert_eq!(hilbert_permutation(&keys), vec![2, 3, 5, 1, 0, 4]);
}

#[test]
fn wkb_bounds_cover_every_coordinate_and_skip_empties() {
    assert_eq!(wkb_bounds(&square(10., 20., 4.)).unwrap(), Some([10., 20., 14., 24.]));
    assert_eq!(wkb_bounds(&point(1., 2.)).unwrap(), Some([1., 2., 1., 2.]));
    assert_eq!(wkb_bounds(&empty_polygon()).unwrap(), None);
    assert_eq!(wkb_bounds(&point(f64::NAN, f64::NAN)).unwrap(), None);
    // ISO Z point: Z is not part of the bounds; the type name says Z.
    let z = [&[1u8][..], &1001u32.to_le_bytes(), &3f64.to_le_bytes(), &4f64.to_le_bytes(), &99f64.to_le_bytes()].concat();
    assert_eq!(wkb_bounds(&z).unwrap(), Some([3., 4., 3., 4.]));
    assert_eq!(wkb_type_name(&z).unwrap(), "Point Z");
    // A multipolygon of two squares.
    let multi = [&[1u8][..], &6u32.to_le_bytes(), &2u32.to_le_bytes(), &square(0., 0., 1.), &square(5., 5., 1.)].concat();
    assert_eq!(wkb_bounds(&multi).unwrap(), Some([0., 0., 6., 6.]));
    // Truncated and unknown input is refused, not guessed.
    assert!(wkb_bounds(&square(0., 0., 1.)[..30]).is_err());
    assert!(wkb_bounds(&[1u8, 9, 0, 0, 0]).is_err());
    assert!(wkb_bounds(&[1u8, 2, 0, 0, 0, 255, 255, 255, 255]).is_err());
}

#[test]
fn archive_profile_unions_layers_with_covering_hilbert_and_layer_groups() {
    let cells = layer(
        "cells",
        ("min_elevation", Arc::new(Float32Array::from(vec![Some(1.5), None, Some(3.0)]))),
        vec![Some(square(100., 100., 8.)), Some(square(0., 0., 4.)), Some(square(50., 50., 2.))],
    );
    let points = layer("points", ("active", Arc::new(BooleanArray::from(vec![true, false]))), vec![Some(point(7., 9.)), None]);
    let mut options = GeoParquetWriteOptions::archive();
    options.row_group_rows = 2;
    let mut out = Vec::new();
    let report = write_arrow_layers(vec![cells, points], &mut out, &options).unwrap();
    assert_eq!((report.rows, report.layers, report.row_groups), (5, 2, 3));
    assert_eq!(report.bbox, Some([0., 0., 108., 108.]));
    let (schema, batch, geo, groups) = read(&out);
    // Row groups of 2 rows that also end at the layer boundary.
    assert_eq!(groups, [2, 1, 2]);
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["mesh_name", "cell_id", "min_elevation", "geometry", "active", "layer", "bbox", "hilbert_index"]);
    // Identity columns of every layer are non-nullable; a column missing from a layer is null.
    let nullable: Vec<bool> = schema.fields().iter().map(|f| f.is_nullable()).collect();
    assert_eq!(nullable, [false, false, true, true, true, false, true, true]);
    assert_eq!(schema.field_with_name("min_elevation").unwrap().data_type(), &DataType::Float32);
    assert_eq!(
        schema.field_with_name("geometry").unwrap().metadata().get("ARROW:extension:name").map(String::as_str),
        Some("geoarrow.wkb")
    );
    // Hilbert order within each layer; the empty geometry last.
    assert_eq!(ids(&batch), [1, 2, 0, 0, 1]);
    assert_eq!(strings(&batch, "layer"), ["cells", "cells", "cells", "points", "points"]);
    let bbox = batch.column_by_name("bbox").unwrap().as_struct();
    assert!(bbox.is_null(4) && bbox.is_valid(0));
    assert_eq!(bbox.column(2).as_primitive::<Float64Type>().value(0), 4.0);
    let h = batch.column_by_name("hilbert_index").unwrap().as_primitive::<arrow_array::types::UInt64Type>();
    let keys = hilbert_keys(&[Some([100., 100., 108., 108.]), Some([0., 0., 4., 4.]), Some([50., 50., 52., 52.])]);
    assert_eq!(h.value(0), u64::from(keys[1].unwrap()));
    assert!(h.is_null(4));
    let column = &geo["columns"]["geometry"];
    assert_eq!(geo["version"], "1.1.0");
    assert_eq!(column["geometry_types"], json!(["Point", "Polygon"]));
    assert_eq!(column["bbox"], json!([0.0, 0.0, 108.0, 108.0]));
    assert_eq!(column["covering"]["bbox"]["xmin"], json!(["bbox", "xmin"]));
    assert_eq!(column["crs"], json!({"id": {"authority": "EPSG", "code": 26918}}));
}

#[test]
fn layout_encoding_options() {
    let cells = || {
        layer(
            "cells",
            ("note", Arc::new(StringArray::from(vec!["a", "b", "c"]))),
            vec![Some(square(0., 0., 1.)), Some(square(2., 2., 1.)), Some(empty_polygon())],
        )
    };
    let column_chunks = |options: &GeoParquetWriteOptions| {
        let mut out = Vec::new();
        write_arrow_layers(vec![cells()], &mut out, options).unwrap();
        let reader = SerializedFileReader::new(bytes::Bytes::from(out)).unwrap();
        let meta = reader.metadata().clone();
        let group = meta.row_group(0);
        let chunks: Vec<(String, bool, bool, Compression)> = group
            .columns()
            .iter()
            .map(|c| (c.column_path().string(), c.dictionary_page_offset().is_some(), c.statistics().is_some(), c.compression()))
            .collect();
        (chunks, meta.offset_index().is_some(), meta.file_metadata().schema_descr().root_schema().name().to_string())
    };
    let (chunks, page_index, root) = column_chunks(&GeoParquetWriteOptions::archive());
    assert_eq!(root, "schema");
    assert!(!page_index);
    let dictionary: Vec<&str> = chunks.iter().filter(|c| c.1).map(|c| c.0.as_str()).collect();
    assert_eq!(dictionary, ["mesh_name", "note", "layer"]);
    // The level is not stored in the file.
    assert!(chunks.iter().all(|c| matches!(c.3, Compression::ZSTD(_))));
    // Flat covering columns, no layer column, no geometry statistics, input order.
    let options = GeoParquetWriteOptions {
        bbox_covering: BboxCovering::Columns,
        bbox_column: "bbox".into(),
        geometry_statistics: false,
        ..GeoParquetWriteOptions::default()
    };
    let (chunks, _, _) = column_chunks(&options);
    let names: Vec<&str> = chunks.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(names, ["mesh_name", "cell_id", "note", "geometry", "bbox_xmin", "bbox_ymin", "bbox_xmax", "bbox_ymax"]);
    assert!(!chunks[3].2, "geometry statistics are off");
    assert!(chunks.iter().all(|c| c.3 == Compression::SNAPPY));
    let mut out = Vec::new();
    write_arrow_layers(vec![cells()], &mut out, &options).unwrap();
    let (_, batch, geo, _) = read(&out);
    assert_eq!(ids(&batch), [0, 1, 2]);
    assert!(batch.column_by_name("bbox_xmin").unwrap().is_null(2));
    assert_eq!(geo["columns"]["geometry"]["covering"]["bbox"]["ymax"], json!(["bbox_ymax"]));
}

#[test]
fn lists_and_null_columns_round_trip() {
    let values = ListArray::from_iter_primitive::<Float64Type, _, _>(vec![Some(vec![Some(1.0), Some(2.0)]), None]);
    let typed = layer("a", ("stations", Arc::new(values)), vec![Some(point(0., 0.)), Some(point(1., 1.))]);
    let untyped = layer("b", ("stations", Arc::new(arrow_array::NullArray::new(1))), vec![Some(point(2., 2.))]);
    let mut out = Vec::new();
    write_arrow_layers(vec![untyped, typed], &mut out, &GeoParquetWriteOptions::archive()).unwrap();
    let (schema, batch, _, groups) = read(&out);
    assert_eq!(groups, [1, 2]);
    let DataType::List(item) = schema.field_with_name("stations").unwrap().data_type() else { panic!("not a list") };
    assert_eq!(item.name(), "element");
    let stations = batch.column_by_name("stations").unwrap().as_list::<i32>();
    assert!(stations.is_null(0));
    assert_eq!(stations.value(1).as_primitive::<Float64Type>().values().to_vec(), [1.0, 2.0]);
}

#[test]
fn split_by_column_keeps_first_appearance_order() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("geometry", DataType::Binary, true),
        Field::new("layer", DataType::Utf8, false),
    ]));
    let batch = |ids: Vec<i64>, layers: Vec<&str>| {
        let wkb: BinaryArray = ids.iter().map(|&i| Some(point(i as f64, 0.0))).collect();
        RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(ids)), Arc::new(wkb), Arc::new(StringArray::from(layers))])
            .unwrap()
    };
    let batches = [batch(vec![0, 1, 2], vec!["b", "b", "a"]), batch(vec![3, 4], vec!["b", "a"])];
    let layers = split_by_column(&schema, &batches, "layer").unwrap();
    let summary: Vec<(String, Vec<i64>)> = layers
        .iter()
        .map(|l| {
            let ids = l
                .batches
                .iter()
                .flat_map(|b| b.as_ref().unwrap().column(0).as_primitive::<arrow_array::types::Int64Type>().values().to_vec())
                .collect();
            (l.name.clone().unwrap(), ids)
        })
        .collect();
    assert_eq!(summary, [("b".to_string(), vec![0, 1, 3]), ("a".to_string(), vec![2, 4])]);
    assert_eq!(layers[0].schema.fields().len(), 2);
    let mut options = GeoParquetWriteOptions::archive();
    options.crs = Some(json!("EPSG:4326"));
    let mut out = Vec::new();
    write_arrow_layers(layers, &mut out, &options).unwrap();
    let (schema, batch, geo, groups) = read(&out);
    assert_eq!(groups, [3, 2]);
    assert_eq!(strings(&batch, "layer"), ["b", "b", "b", "a", "a"]);
    assert!(!schema.field_with_name("id").unwrap().is_nullable());
    assert_eq!(geo["columns"]["geometry"]["crs"], json!("EPSG:4326"));
}

#[test]
fn refusals() {
    let one = || layer("a", ("x", Arc::new(Int64Array::from(vec![1]))), vec![Some(point(0., 0.))]);
    let write = |layers: Vec<MemoryLayer>, options: &GeoParquetWriteOptions| write_arrow_layers(layers, Vec::new(), options).map(|_| ());
    // Several layers need a layer column, and unique names.
    assert!(
        write(vec![one(), layer("b", ("x", Arc::new(Int64Array::from(vec![1]))), vec![None])], &GeoParquetWriteOptions::default()).is_err()
    );
    assert!(write(vec![one(), one()], &GeoParquetWriteOptions::archive()).is_err());
    // Different types for one field.
    let other = layer("b", ("x", Arc::new(StringArray::from(vec!["1"]))), vec![None]);
    assert!(write(vec![one(), other], &GeoParquetWriteOptions::archive()).is_err());
    // A generated column colliding with an input column.
    let clash = layer("a", ("BBOX", Arc::new(Int64Array::from(vec![1]))), vec![None]);
    assert!(write(vec![clash], &GeoParquetWriteOptions::archive()).is_err());
    // A Hilbert column without Hilbert order.
    let options = GeoParquetWriteOptions { hilbert_column: Some("h".into()), ..GeoParquetWriteOptions::default() };
    assert!(write(vec![one()], &options).is_err());
    // Nulls in an identity column.
    let nulls = layer("a", ("x", Arc::new(Int64Array::from(vec![1]))), vec![None]).with_identity(["x", "mesh_name"]);
    let mut nulls = nulls;
    nulls.batches = vec![Ok(RecordBatch::try_new(
        nulls.schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![None::<&str>])),
            Arc::new(Int64Array::from(vec![0])),
            Arc::new(Int64Array::from(vec![1])),
            Arc::new(BinaryArray::from(vec![None::<&[u8]>])),
        ],
    )
    .unwrap())];
    assert!(write(vec![nulls], &GeoParquetWriteOptions::default()).is_err());
    // Bad WKB, and a bad ZSTD level.
    let bad = layer("a", ("x", Arc::new(Int64Array::from(vec![1]))), vec![Some(vec![1, 2, 3])]);
    assert!(write(vec![bad], &GeoParquetWriteOptions::default()).is_err());
    let options = GeoParquetWriteOptions { compression: ParquetCompression::Zstd(99), ..GeoParquetWriteOptions::default() };
    assert!(write(vec![one()], &options).is_err());
    assert!(ParquetCompression::parse("snappy", Some(3)).is_err());
    assert_eq!(ParquetCompression::parse("zstd", Some(3)).unwrap(), ParquetCompression::Zstd(3));
}

#[test]
fn path_writer_replaces_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.parquet");
    std::fs::write(&path, b"old").unwrap();
    let cells = layer("a", ("x", Arc::new(Int64Array::from(vec![1]))), vec![Some(point(0., 0.))]);
    write_arrow_layers_to_path(vec![cells], path.to_str().unwrap(), &GeoParquetWriteOptions::archive()).unwrap();
    let (_, batch, _, _) = read(&std::fs::read(&path).unwrap());
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no staged file is left behind");
}

#[test]
fn open_sniffs_geoparquet_without_its_extension() {
    // terrio's own driver reads it (wbvector's GeoParquet is not built).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cells.data");
    let cells = layer("a", ("x", Arc::new(Int64Array::from(vec![7]))), vec![Some(point(1., 2.))]);
    let options =
        GeoParquetWriteOptions { crs: Some(json!({"id": {"authority": "EPSG", "code": 26918}})), ..GeoParquetWriteOptions::default() };
    write_arrow_layers_to_path(vec![cells], path.to_str().unwrap(), &options).unwrap();
    // Detection by content goes through the fallback, which is off by default (#444).
    crate::config::push_scope([("TERRIO_WBVECTOR_FALLBACK".to_string(), Some("YES".to_string()))].into());
    let opened = crate::open(path.to_str().unwrap());
    crate::config::pop_scope();
    let crate::Dataset::Vector(ds) = opened.unwrap() else { panic!("not a vector dataset") };
    assert_eq!(ds.layers[0].len(), 1);
    assert_eq!(ds.layers[0].features[0].geometry, Some(wbvector::Geometry::point(1., 2.)));
}

/// A layer of 70,000 polygons with a repetitive string column: two row groups of the archive's
/// 65,536 rows.
fn many_cells() -> MemoryLayer {
    let n = 70_000;
    let wkb: Vec<Option<Vec<u8>>> = (0..n).map(|i| Some(square(f64::from(i % 300), f64::from(i / 300), 1.0))).collect();
    let notes = StringArray::from_iter_values((0..n).map(|i| format!("note {}", i % 7)));
    layer("cells", ("note", Arc::new(notes)), wkb)
}

#[test]
fn zstd_pages_decompress_to_the_uncompressed_files_columns() {
    use parquet::file::serialized_reader::ReadOptionsBuilder;
    let write = |compression, statistics| {
        let options = GeoParquetWriteOptions { compression, statistics, ..GeoParquetWriteOptions::archive() };
        let mut out = Vec::new();
        write_arrow_layers(vec![many_cells()], &mut out, &options).unwrap();
        out
    };
    let plain = write(ParquetCompression::Uncompressed, ColumnStatistics::Page);
    let (schema_a, a, geo_a, groups_a) = read(&plain);
    assert_eq!(groups_a, [65536, 4464]);
    for statistics in [ColumnStatistics::Page, ColumnStatistics::Chunk] {
        for level in [1, 3, 9, 19] {
            let zstd = write(ParquetCompression::Zstd(level), statistics);
            assert!(zstd.len() * 3 < plain.len(), "level {level}: {} vs {}", zstd.len(), plain.len());
            let (schema_b, b, geo_b, groups_b) = read(&zstd);
            assert_eq!((&schema_a, &geo_a, &groups_a), (&schema_b, &geo_b, &groups_b));
            assert_eq!(a, b);
            // Every chunk says ZSTD, and the page index points at the pages actually written.
            let options = ReadOptionsBuilder::new().with_page_index().build();
            let reader = SerializedFileReader::new_with_options(bytes::Bytes::from(zstd.clone()), options).unwrap();
            let meta = reader.metadata();
            for group in meta.row_groups() {
                assert!(group.columns().iter().all(|c| matches!(c.compression(), Compression::ZSTD(_))));
            }
            match (statistics, meta.offset_index()) {
                (ColumnStatistics::Page, Some(index)) => {
                    for (g, group) in index.iter().enumerate() {
                        for (c, chunk) in group.iter().enumerate() {
                            let column = meta.row_group(g).column(c);
                            let first = chunk.page_locations()[0].offset;
                            assert_eq!(first, column.data_page_offset());
                            let end = column.dictionary_page_offset().unwrap_or(first) + column.compressed_size();
                            let last = chunk.page_locations().last().unwrap();
                            assert_eq!(last.offset + i64::from(last.compressed_page_size), end);
                        }
                    }
                }
                (ColumnStatistics::Chunk, None) => {}
                (s, i) => panic!("{s:?}: page index {}", i.is_some()),
            }
        }
    }
}

/// terrio's Zstandard pages (`ruzstd`, or the C library with `libzstd`) and the `parquet` crate's
/// (the zstd C library): the same file but for the compressed bytes. Each decodes with the other
/// implementation to the same rows; pages, encodings, dictionaries, statistics and page index
/// entries are identical.
#[cfg(feature = "parquet-zstd")]
#[test]
fn terrio_zstd_pages_match_the_zstd_c_librarys_but_for_their_bytes() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::file::serialized_reader::ReadOptionsBuilder;
    let crate_read = |bytes: &[u8]| {
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes)).unwrap();
        let schema = builder.schema().clone();
        let batches: Vec<RecordBatch> = builder.build().unwrap().map(|b| b.unwrap()).collect();
        arrow_select::concat::concat_batches(&schema, &batches).unwrap()
    };
    let chunks = |bytes: &[u8]| {
        let options = ReadOptionsBuilder::new().with_page_index().build();
        let reader = SerializedFileReader::new_with_options(bytes::Bytes::copy_from_slice(bytes), options).unwrap();
        let meta = reader.metadata().clone();
        let mut out = Vec::new();
        for (g, group) in meta.row_groups().iter().enumerate() {
            for (i, column) in group.columns().iter().enumerate() {
                let pages =
                    meta.offset_index().map(|index| index[g][i].page_locations().iter().map(|p| p.first_row_index).collect::<Vec<_>>());
                out.push((
                    column.column_path().string(),
                    column.compression(),
                    format!("{:?}", column.encodings().collect::<Vec<_>>()),
                    column.dictionary_page_offset().is_some(),
                    format!("{:?}", column.statistics()),
                    column.num_values(),
                    pages,
                ));
            }
        }
        (out, meta.column_index().map(|c| format!("{c:?}")))
    };
    // Every page, decompressed by the parquet crate (the C library): type, values, encoding,
    // statistics and bytes. (A chunk's uncompressed size can still differ by a byte or so: it
    // counts the page headers, which hold the compressed page size.)
    let pages = |bytes: &[u8]| {
        let reader = SerializedFileReader::new(bytes::Bytes::copy_from_slice(bytes)).unwrap();
        let mut out = Vec::new();
        for g in 0..reader.num_row_groups() {
            let group = reader.get_row_group(g).unwrap();
            for c in 0..group.num_columns() {
                let mut pages = group.get_column_page_reader(c).unwrap();
                while let Some(p) = pages.get_next_page().unwrap() {
                    out.push((g, c, p.page_type(), p.num_values(), p.encoding(), format!("{:?}", p.statistics()), p.buffer().to_vec()));
                }
            }
        }
        out
    };
    for statistics in [ColumnStatistics::Page, ColumnStatistics::Chunk] {
        let options = GeoParquetWriteOptions { statistics, ..GeoParquetWriteOptions::archive() };
        let write = |terrio_zstd| {
            let codec = options.compression.codec(terrio_zstd).unwrap();
            write_layers_with(vec![many_cells()], Vec::new(), &options, codec).unwrap().1
        };
        let (ours, c) = (write(true), write(false));
        // With `libzstd` terrio compresses with the same C library at the same level: the same
        // file, byte for byte. Without it (`ruzstd`), only the compressed bytes differ.
        assert_eq!(ours == c, cfg!(feature = "libzstd"));
        // Decoded by terrio and by the parquet crate (the C library): the same rows.
        let (a, b) = (read(&ours), read(&c));
        assert_eq!(a, b);
        assert_eq!(crate_read(&ours), a.1);
        assert_eq!(crate_read(&c), a.1);
        assert_eq!(chunks(&ours), chunks(&c), "{statistics:?}");
        let ours_pages = pages(&ours);
        assert!(ours_pages.len() > 20, "{} pages", ours_pages.len());
        assert!(ours_pages == pages(&c), "{statistics:?}: pages differ");
    }
}
