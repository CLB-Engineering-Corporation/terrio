//! Writes the GDB_Items definitions of a geodatabase to <out_dir>: `items <gdb> <out_dir>`.
use terrio::filegdb::{Geodatabase, Value};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let gdb = Geodatabase::open(&a[1])?;
    std::fs::create_dir_all(&a[2])?;
    let t = gdb.open_table("GDB_Items")?;
    let f = |n: &str| t.field_index(n).unwrap();
    let (iu, it, ina, ipn, ip, ist1, ist2, ii1, idef, idoc, iinfo, iprop) = (
        f("UUID"),
        f("Type"),
        f("Name"),
        f("PhysicalName"),
        f("Path"),
        f("DatasetSubtype1"),
        f("DatasetSubtype2"),
        f("DatasetInfo1"),
        f("Definition"),
        f("Documentation"),
        f("ItemInfo"),
        f("Properties"),
    );
    for r in t.rows()? {
        let r = r?;
        let s = |i: usize| match &r.values[i] {
            Value::Null => "<null>".to_string(),
            v => format!("{v:?}"),
        };
        println!(
            "{} uuid={} type={} name={} phys={} path={} st1={} st2={} info1={} props={} doc={} iteminfo={} shape={}",
            r.object_id,
            s(iu),
            s(it),
            s(ina),
            s(ipn),
            s(ip),
            s(ist1),
            s(ist2),
            s(ii1),
            s(iprop),
            if r.values[idoc].is_null() { "null" } else { "set" },
            if r.values[iinfo].is_null() { "null" } else { "set" },
            r.shape().is_some()
        );
        if let Some(d) = r.values[idef].as_str() {
            let name = r.values[ina].as_str().filter(|n| !n.is_empty()).unwrap_or("root");
            std::fs::write(format!("{}/{}_{}.xml", a[2], r.object_id, name), d)?;
        }
        if let Some(d) = r.values[idoc].as_str() {
            std::fs::write(format!("{}/{}_doc.xml", a[2], r.object_id), d)?;
        }
        if let Some(d) = r.values[iinfo].as_str() {
            std::fs::write(format!("{}/{}_iteminfo.xml", a[2], r.object_id), d)?;
        }
    }
    Ok(())
}
