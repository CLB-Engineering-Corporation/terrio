//! Write ordered, sampled vector batches as Arrow IPC, without GeoJSON or a cloned Layer.
//!
//! `cargo run --release -p terrio --example sample_columns -- points.fgb raster.tif output.arrow [8192]`
//! The vector reader still materializes the inventory. Only the sample index and Arrow batches
//! are bounded here. For streaming inventories, call `sample_points` on each incoming batch.

use std::sync::Arc;
use terrio::Geometry;
use terrio::arrow::{ArrowEncoder, ArrowWriteOptions};
use terrio::ops::sample::{PointSampleOptions, sample_points};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(4..=5).contains(&args.len()) {
        return Err("usage: sample_columns POINTS RASTER OUTPUT.arrow [BATCH_ROWS]".into());
    }
    let size = args.get(4).map(|s| s.parse::<usize>()).transpose()?.unwrap_or(8192);
    if size == 0 {
        return Err("batch rows must be positive".into());
    }
    let points = terrio::open_vector(&args[1], &Default::default())?;
    if points.layers.len() != 1 {
        return Err("select a single point layer".into());
    }
    let layer = &points.layers[0];
    let options = ArrowWriteOptions::default();
    let encoder = ArrowEncoder::for_layer(layer, &options)?;
    let mut fields: Vec<_> = encoder.schema().fields().iter().cloned().collect();
    for (name, ty, nullable) in [
        ("sample_fid", arrow_schema::DataType::UInt64, false),
        ("sample_value", arrow_schema::DataType::Float64, true),
        ("sample_status", arrow_schema::DataType::Utf8, false),
    ] {
        if fields.iter().any(|f| f.name() == name) {
            return Err(format!("output field collision: {name}").into());
        }
        fields.push(Arc::new(arrow_schema::Field::new(name, ty, nullable)));
    }
    let schema = Arc::new(arrow_schema::Schema::new(fields));
    let mut raster = terrio::open_raster_lazy(&args[2])?;
    let src = terrio::crs::SpatialRef::from_layer(layer);
    // This example refuses overwrites; callers should stage and publish only after success.
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&args[3])?;
    let mut writer = arrow_ipc::writer::FileWriter::try_new(file, &schema)?;
    let mut sample_seconds = 0.0;
    for chunk in layer.features.chunks(size) {
        let xy = chunk
            .iter()
            .map(|f| match &f.geometry {
                Some(Geometry::Point(p)) => Ok(Some((p.x, p.y))),
                None => Ok(None),
                _ => Err("sampling requires point geometries"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let start = std::time::Instant::now();
        let samples =
            sample_points(&mut raster, 1, &src, &xy, &PointSampleOptions { strict_transform_errors: true, ..Default::default() })?;
        sample_seconds += start.elapsed().as_secs_f64();
        let mut columns = encoder.encode(chunk)?.columns().to_vec();
        columns.push(Arc::new(arrow_array::UInt64Array::from_iter_values(chunk.iter().map(|f| f.fid))));
        columns.push(Arc::new(arrow_array::Float64Array::from(samples.values)));
        columns.push(Arc::new(arrow_array::StringArray::from_iter_values(samples.statuses.iter().map(|s| s.as_str()))));
        writer.write(&arrow_array::RecordBatch::try_new(schema.clone(), columns)?)?;
    }
    writer.finish()?;
    eprintln!("rows={} sample_seconds={sample_seconds:.6}", layer.len());
    Ok(())
}
