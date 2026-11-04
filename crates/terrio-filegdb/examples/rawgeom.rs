//! Prints raw geometry blob bytes of the first N rows: `rawgeom <gdb> <table> [n]`.
use terrio_filegdb::{Geodatabase, RowFilter, Value};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let gdb = Geodatabase::open(&a[1])?;
    let t = gdb.open_table(&a[2])?;
    let n: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let gi = t.geometry_field_index().unwrap();
    for r in t.rows_filtered(RowFilter::default())?.take(n) {
        let r = r?;
        if let Value::Geometry(s) = &r.values[gi] {
            println!("{} {:?}", r.object_id, s);
        }
    }
    Ok(())
}
