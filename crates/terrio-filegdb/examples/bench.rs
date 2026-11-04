//! Times a full read and a bbox-filtered read: `bench <gdb> <table> xmin ymin xmax ymax`.
use terrio_filegdb::{Geodatabase, RowFilter};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let t = Geodatabase::open(&a[1])?.open_table(&a[2])?;
    let t0 = std::time::Instant::now();
    let n = t.rows()?.count();
    println!("full read: {n} rows in {:.2}s", t0.elapsed().as_secs_f64());
    let b: Vec<f64> = a[3..7].iter().map(|s| s.parse().unwrap()).collect();
    let t0 = std::time::Instant::now();
    let n = t.rows_filtered(RowFilter { columns: None, bbox: Some([b[0], b[1], b[2], b[3]]) })?.count();
    println!("bbox read: {n} rows in {:.2}s", t0.elapsed().as_secs_f64());
    Ok(())
}
