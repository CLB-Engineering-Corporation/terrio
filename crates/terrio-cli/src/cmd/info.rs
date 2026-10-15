//! `terrio info`.

use terrio::info::InfoOptions;

#[derive(clap::Args)]
pub struct Args {
    /// The dataset: a path, URL, `s3://` or `gs://` object, or zip archive.
    input: String,
    /// Print JSON.
    #[arg(long)]
    json: bool,
    /// Compute band statistics; for point clouds, read every point to count classifications and
    /// returns and give each dimension's range.
    #[arg(long)]
    stats: bool,
    /// Compute band histograms as `gdalinfo -hist` does: 256 buckets over the type's range for
    /// Byte and Int8 bands, otherwise over the band's approximate minimum and maximum (exact with
    /// `--stats`) widened by half a bucket.
    #[arg(long)]
    hist: bool,
    /// Only these layers of a vector dataset.
    #[arg(short, long = "layer")]
    layers: Vec<String>,
    /// Open the input with this driver: `gdal` uses the GDAL backend (builds with the `gdal`
    /// feature), as a `GDAL:` prefix on the path does.
    #[arg(long, value_name = "DRIVER")]
    driver: Option<String>,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let options = InfoOptions { statistics: a.stats, histogram: a.hist };
    let input = super::with_driver(&a.input, a.driver.as_deref())?;
    let info = if a.layers.is_empty() {
        // GeoTIFFs are read in place: only their headers are fetched.
        super::quiet(|| terrio::info::info_path(&input, &options))?
    } else {
        let ds =
            terrio::Dataset::Vector(terrio::open_vector(&input, &terrio::VectorOpenOptions { layers: a.layers, ..Default::default() })?);
        terrio::info::info(&ds, &options)
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&info).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    } else {
        print!("{}", info.to_text());
    }
    Ok(())
}
