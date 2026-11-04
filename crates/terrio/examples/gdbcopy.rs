//! Copies the user tables, coded-value domains and field-domain links of a
//! File Geodatabase into a new one written by [`GdbWriter`]:
//! `gdbcopy <src.gdb> <dst.gdb>`. Used to validate the writer against ArcGIS.
use terrio::filegdb::{FieldDef, FieldType, GdbWriter, Geodatabase, GeometryDef, TableDef, Value};

/// Text between `<tag>` and `</tag>` occurrences (crude, enough for Esri XML).
fn tags<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        if !after.starts_with('>') && !after.starts_with(' ') {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let src = Geodatabase::open(&a[1])?;
    let mut w = GdbWriter::create(&a[2])?;
    let items = src.open_table("GDB_Items")?;
    let idef = items.field_index("Definition").unwrap();
    let mut table_xml = std::collections::HashMap::new();
    for r in items.rows()? {
        let r = r?;
        let Some(def) = r.values[idef].as_str() else {
            continue;
        };
        if def.starts_with("<GPCodedValueDomain2") {
            let name = unescape(tags(def, "DomainName")[0]);
            let desc = unescape(tags(def, "Description").first().copied().unwrap_or(""));
            let ft = match tags(def, "FieldType")[0] {
                "esriFieldTypeString" => FieldType::String,
                "esriFieldTypeSmallInteger" => FieldType::Int16,
                "esriFieldTypeInteger" => FieldType::Int32,
                _ => FieldType::Float64,
            };
            let vals: Vec<(String, String)> =
                tags(def, "CodedValue").iter().map(|cv| (unescape(tags(cv, "Code")[0]), unescape(tags(cv, "Name")[0]))).collect();
            let refs: Vec<(&str, &str)> = vals.iter().map(|(c, n)| (c.as_str(), n.as_str())).collect();
            w.add_coded_domain(&name, &desc, ft, &refs)?;
        } else if let Some(n) = tags(def, "Name").first() {
            table_xml.insert(unescape(n), def.to_string());
        }
    }
    let mut names = src.table_names();
    names.sort();
    for name in names {
        let t = src.open_table(&name)?;
        let xml = table_xml.get(&name).cloned().unwrap_or_default();
        let domain_of = |field: &str| -> Option<String> {
            tags(&xml, "GPFieldInfoEx")
                .iter()
                .find(|f| tags(f, "Name").first().is_some_and(|n| unescape(n) == field))
                .and_then(|f| tags(f, "DomainName").first().map(|d| unescape(d)))
        };
        // ArcGIS shows the item XML alias, which can differ from the one in
        // the .gdbtable field descriptor.
        let alias_of = |field: &str, fallback: &str| -> String {
            tags(&xml, "GPFieldInfoEx")
                .iter()
                .find(|f| tags(f, "Name").first().is_some_and(|n| unescape(n) == field))
                .and_then(|f| tags(f, "AliasName").first().map(|d| unescape(d)))
                .unwrap_or_else(|| fallback.to_string())
        };
        let managed = ["OBJECTID", "SHAPE", "SHAPE_LENGTH", "SHAPE_AREA"];
        let keep: Vec<usize> = (0..t.fields().len())
            .filter(|&i| {
                let f = &t.fields()[i];
                !managed.contains(&f.name.to_ascii_uppercase().as_str())
                    && !matches!(f.field_type, FieldType::ObjectId | FieldType::Geometry)
            })
            .collect();
        let fields: Vec<FieldDef> = keep
            .iter()
            .map(|&i| {
                let f = &t.fields()[i];
                FieldDef {
                    name: f.name.clone(),
                    alias: alias_of(&f.name, &f.alias),
                    field_type: f.field_type,
                    nullable: f.nullable,
                    length: f.length,
                    domain: domain_of(&f.name),
                }
            })
            .collect();
        let geometry = t.geometry_info().map(|g| {
            let mut d = GeometryDef::epsg_2278(g.shape_type);
            d.x_origin = g.x_origin;
            d.y_origin = g.y_origin;
            d.xy_scale = g.xy_scale;
            d.xy_tolerance = g.xy_tolerance;
            d
        });
        w.create_table(&TableDef { name: name.clone(), geometry, fields })?;
        let mut n = 0;
        for r in t.rows()? {
            let r = r?;
            let vals: Vec<Value> = keep.iter().map(|&i| r.values[i].clone()).collect();
            w.insert(&name, r.shape(), vals)?;
            n += 1;
        }
        println!("{name}: {n} rows");
    }
    w.finish()?;
    Ok(())
}
