//! Dumps a File Geodatabase: `gdbdump <path.gdb> [table] [n_rows]`.
use terrio::filegdb::{Geodatabase, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: gdbdump <path.gdb> [table] [n_rows]");
        std::process::exit(2);
    };
    let gdb = Geodatabase::open(path)?;
    let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let names = match args.get(2) {
        Some(t) => vec![t.clone()],
        None => gdb.table_names(),
    };
    for name in names {
        let t = gdb.open_table(&name)?;
        println!("== {} ({} rows)", t.name(), t.feature_count());
        if let Some(g) = t.geometry_info() {
            println!(
                "  geometry: {:?} z={} m={} origin=({}, {}) scale={} tol={} extent={:?}",
                g.shape_type, g.has_z, g.has_m, g.x_origin, g.y_origin, g.xy_scale, g.xy_tolerance, g.extent
            );
        }
        for f in t.fields() {
            println!("  {:<28} {:?} nullable={} len={} alias={}", f.name, f.field_type, f.nullable, f.length, f.alias);
        }
        for row in t.rows()?.take(n) {
            let row = row?;
            let vals: Vec<String> = row
                .values
                .iter()
                .map(|v| match v {
                    Value::Geometry(s) => format!(
                        "<{:?} parts={} pts={} area={:.4} len={:.4}>",
                        s.shape_type,
                        s.parts.len(),
                        s.parts.iter().map(Vec::len).sum::<usize>(),
                        s.area(),
                        s.length()
                    ),
                    other => format!("{other:?}"),
                })
                .collect();
            println!("  [{}] {}", row.object_id, vals.join(" | "));
        }
    }
    Ok(())
}
