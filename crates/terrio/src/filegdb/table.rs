// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable.cpp, filegdbtable_priv.h

//! `.gdbtable` / `.gdbtablx` reading.
//!
//! Ported from GDAL's `FileGDBTable::Open`, `ReadTableXHeaderV3/V4`,
//! `GetOffsetInTableForRow`, `SelectRow` and `GetFieldValue`
//! (`ogr/ogrsf_frmts/openfilegdb/filegdbtable.cpp`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::filegdb::varint::{Cursor, utf16};
use crate::filegdb::{Error, FieldInfo, FieldType, GeometryFieldInfo, Result, Row, ShapeType, Value, geometry};

/// Raster field storage kind (needed only to skip raster values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RasterKind {
    External,
    Managed,
    Inline,
}

/// How rows are located: via the `.gdbtablx` offsets.
#[derive(Debug)]
struct TablX {
    data: Vec<u8>,
    offset_size: usize,
    /// Logical row count (max OBJECTID).
    total: u64,
    /// Bitmap of present 1024-row blocks (empty = all present).
    block_map: Vec<u8>,
}

/// A table (or feature class) of a File Geodatabase.
#[derive(Debug)]
pub struct Table {
    name: String,
    path: PathBuf,
    fields: Vec<FieldInfo>,
    raster_kinds: Vec<Option<RasterKind>>,
    geom_index: Option<usize>,
    geom: Option<GeometryFieldInfo>,
    grid: Vec<f64>,
    strings_utf8: bool,
    valid_count: u64,
    nullable_bytes: usize,
    tablx: Option<TablX>,
    data: OnceLock<std::result::Result<Vec<u8>, String>>,
}

/// Options for [`Table::rows_filtered`].
#[derive(Debug, Clone, Default)]
pub struct RowFilter {
    /// Only decode these field indices (others are `Value::Null`). `None` = all.
    pub columns: Option<Vec<usize>>,
    /// Skip rows whose geometry envelope does not intersect this envelope
    /// (xmin, ymin, xmax, ymax). Rows with null geometry are skipped too.
    pub bbox: Option<[f64; 4]>,
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
    Error::Io { path: path.to_path_buf(), source: e }
}

fn test_bit(arr: &[u8], i: usize) -> bool {
    arr.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
}

fn read_tablx(path: &Path, version: i32) -> Result<TablX> {
    let data = std::fs::read(path).map_err(|e| io_err(path, e))?;
    let mut c = Cursor::new(&data);
    let v = c.i32()?;
    if v != version {
        return Err(Error::Format(format!(".gdbtablx version is {v}, expected {version}")));
    }
    let (blocks, total, offset_size, block_map);
    if version == 3 {
        blocks = u64::from(c.u32()?);
        let t = c.i32()?;
        offset_size = c.u32()? as usize;
        if !(4..=6).contains(&offset_size) {
            return Err(Error::Format(format!("invalid .gdbtablx offset size {offset_size}")));
        }
        total = u64::try_from(t).map_err(|_| Error::Format("negative record count".into()))?;
        let trailer = 16 + offset_size * 1024 * blocks as usize;
        let mut bm = Vec::new();
        if blocks != 0 {
            let mut tc = Cursor::new(&data);
            tc.skip(trailer)?;
            let bitmap_words = tc.u32()?;
            let bits_for_block_map = tc.u32()? as usize;
            let blocks_bis = u64::from(tc.u32()?);
            tc.u32()?;
            if blocks_bis != blocks {
                return Err(Error::Format("inconsistent .gdbtablx trailer".into()));
            }
            if bitmap_words != 0 {
                bm = tc.bytes(bits_for_block_map.div_ceil(8))?.to_vec();
            }
        }
        block_map = bm;
    } else {
        blocks = u64::from_le_bytes(data.get(4..12).ok_or_else(|| Error::Format("short .gdbtablx".into()))?.try_into().expect("8"));
        let mut hc = Cursor::new(&data);
        hc.skip(12)?;
        offset_size = hc.u32()? as usize;
        if !(4..=6).contains(&offset_size) {
            return Err(Error::Format(format!("invalid .gdbtablx offset size {offset_size}")));
        }
        let trailer = 16 + offset_size * 1024 * blocks as usize;
        let mut bm = Vec::new();
        let mut t = 0;
        if blocks != 0 {
            let mut tc = Cursor::new(&data);
            tc.skip(trailer)?;
            t = tc.i64()? as u64;
            let section = tc.u32()? as usize;
            if section == 22 + 32768 + 52 {
                let s = tc.bytes(section)?;
                if s[..6] == [1, 0, 1, 0, 0, 0] && s[22 + 32768..22 + 32768 + 12] == [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] {
                    bm = s[22..22 + 32768].to_vec();
                } else {
                    return Err(Error::Unsupported("unrecognised version-4 .gdbtablx block map".into()));
                }
            } else if section != 0 {
                return Err(Error::Unsupported("unrecognised version-4 .gdbtablx block map".into()));
            }
        }
        total = t;
        block_map = bm;
    }
    Ok(TablX { data, offset_size, total, block_map })
}

impl TablX {
    /// Byte offset of row `i_row` (0-based logical row = OBJECTID - 1) in the
    /// `.gdbtable`; 0 if the row does not exist / is deleted.
    fn offset(&self, i_row: u64, prefix_blocks: &[u32]) -> u64 {
        let corrected = if self.block_map.is_empty() {
            i_row
        } else {
            let block = (i_row / 1024) as usize;
            if !test_bit(&self.block_map, block) {
                return 0;
            }
            u64::from(prefix_blocks[block]) * 1024 + i_row % 1024
        };
        let start = 16 + self.offset_size * corrected as usize;
        let Some(b) = self.data.get(start..start + self.offset_size) else {
            return 0;
        };
        let mut buf = [0u8; 8];
        buf[..self.offset_size].copy_from_slice(b);
        u64::from_le_bytes(buf)
    }

    /// Number of present blocks before each block index.
    fn prefix_blocks(&self) -> Vec<u32> {
        let n = (self.total as usize).div_ceil(1024);
        let mut out = Vec::with_capacity(n);
        let mut acc = 0u32;
        for i in 0..n {
            out.push(acc);
            if test_bit(&self.block_map, i) {
                acc += 1;
            }
        }
        out
    }
}

impl Table {
    /// Opens a `.gdbtable` file (and its `.gdbtablx` sibling).
    pub(crate) fn open_file(path: &Path, name: &str) -> Result<Self> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| io_err(path, e))?;
        let mut header = [0u8; 40];
        f.read_exact(&mut header).map_err(|e| io_err(path, e))?;
        let mut h = Cursor::new(&header);
        let version = h.i32()?;
        if version != 3 && version != 4 {
            return Err(Error::Unsupported(format!("FileGDB table version {version}")));
        }
        let valid_count = if version == 3 {
            u64::try_from(i32::from_le_bytes(header[4..8].try_into().expect("4"))).map_err(|_| Error::Format("negative count".into()))?
        } else {
            u64::try_from(i64::from_le_bytes(header[16..24].try_into().expect("8"))).map_err(|_| Error::Format("negative count".into()))?
        };
        let field_desc_offset = u64::from_le_bytes(header[32..40].try_into().expect("8"));

        // Field descriptors.
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(field_desc_offset)).map_err(|e| io_err(path, e))?;
        let mut fd_head = [0u8; 14];
        f.read_exact(&mut fd_head).map_err(|e| io_err(path, e))?;
        let fd_len = u32::from_le_bytes(fd_head[0..4].try_into().expect("4")) as usize;
        if !(10..=10 * 1024 * 1024).contains(&fd_len) {
            return Err(Error::Format(format!("invalid field description length {fd_len}")));
        }
        let strings_utf8 = fd_head[9] & 1 != 0;
        let flags = fd_head[11];
        let table_has_m = flags & (1 << 6) != 0;
        let table_has_z = flags & (1 << 7) != 0;
        let table_geom_type = fd_head[8];
        let n_fields = u16::from_le_bytes([fd_head[12], fd_head[13]]) as usize;
        let mut fd = vec![0u8; fd_len - 10];
        f.read_exact(&mut fd).map_err(|e| io_err(path, e))?;
        drop(f);

        let mut c = Cursor::new(&fd);
        let mut fields = Vec::with_capacity(n_fields);
        let mut raster_kinds = Vec::with_capacity(n_fields);
        let mut geom_index = None;
        let mut grid = Vec::new();
        let mut geom = None;
        for _ in 0..n_fields {
            let n = c.u8()? as usize;
            let fname = utf16(c.bytes(2 * n)?);
            let n = c.u8()? as usize;
            let alias = utf16(c.bytes(2 * n)?);
            let ty = c.u8()?;
            let field_type = match ty {
                0 => FieldType::Int16,
                1 => FieldType::Int32,
                2 => FieldType::Float32,
                3 => FieldType::Float64,
                4 => FieldType::String,
                5 => FieldType::DateTime,
                6 => FieldType::ObjectId,
                7 => FieldType::Geometry,
                8 => FieldType::Binary,
                9 => FieldType::Raster,
                10 => FieldType::Guid,
                11 => FieldType::GlobalId,
                12 => FieldType::Xml,
                13 => FieldType::Int64,
                14 => FieldType::Date,
                15 => FieldType::Time,
                16 => FieldType::DateTimeOffset,
                t => return Err(Error::Unsupported(format!("field type {t} of field {fname}"))),
            };
            let mut length = 0u32;
            let nullable;
            let mut raster_kind = None;
            match field_type {
                FieldType::Geometry | FieldType::Raster => {
                    let _width = c.u8()?;
                    let fl = c.u8()?;
                    nullable = fl & 1 != 0;
                    if field_type == FieldType::Raster {
                        let n = c.u8()? as usize;
                        c.skip(2 * n)?;
                    }
                    let wkt_len = c.u16()? as usize;
                    let wkt = utf16(c.bytes(wkt_len)?);
                    let gflags = c.u8()?;
                    let has_m_ost = gflags & 2 != 0;
                    let has_z_ost = gflags & 4 != 0;
                    let mut gi = GeometryFieldInfo {
                        shape_type: match table_geom_type {
                            1 => ShapeType::Point,
                            2 => ShapeType::Multipoint,
                            3 => ShapeType::Polyline,
                            4 => ShapeType::Polygon,
                            9 => ShapeType::MultiPatch,
                            _ => ShapeType::Polygon,
                        },
                        has_z: table_has_z,
                        has_m: table_has_m,
                        wkt,
                        x_origin: 0.0,
                        y_origin: 0.0,
                        xy_scale: 0.0,
                        z_origin: 0.0,
                        z_scale: 0.0,
                        m_origin: 0.0,
                        m_scale: 0.0,
                        xy_tolerance: 0.0,
                        z_tolerance: 0.0,
                        m_tolerance: 0.0,
                        extent: [0.0; 4],
                    };
                    if field_type == FieldType::Geometry || gflags > 0 {
                        gi.x_origin = c.f64()?;
                        gi.y_origin = c.f64()?;
                        gi.xy_scale = c.f64()?;
                        if gi.xy_scale == 0.0 {
                            return Err(Error::Format("zero xy scale".into()));
                        }
                        if has_m_ost {
                            gi.m_origin = c.f64()?;
                            gi.m_scale = c.f64()?;
                        }
                        if has_z_ost {
                            gi.z_origin = c.f64()?;
                            gi.z_scale = c.f64()?;
                        }
                        gi.xy_tolerance = c.f64()?;
                        if has_m_ost {
                            gi.m_tolerance = c.f64()?;
                        }
                        if has_z_ost {
                            gi.z_tolerance = c.f64()?;
                        }
                    }
                    if field_type == FieldType::Raster {
                        raster_kind = Some(match c.u8()? {
                            0 => RasterKind::External,
                            1 => RasterKind::Managed,
                            _ => RasterKind::Inline,
                        });
                    } else {
                        gi.extent = [c.f64()?, c.f64()?, c.f64()?, c.f64()?];
                        if table_has_z {
                            c.skip(16)?;
                        }
                        if table_has_m {
                            c.skip(16)?;
                        }
                        c.skip(1)?;
                        let n_grid = c.u32()? as usize;
                        if n_grid == 0 || n_grid > 3 {
                            return Err(Error::Format(format!("invalid spatial index grid count {n_grid}")));
                        }
                        grid = (0..n_grid).map(|_| c.f64()).collect::<Result<_>>()?;
                        if geom_index.is_some() {
                            return Err(Error::Format("several geometry fields".into()));
                        }
                        geom_index = Some(fields.len());
                        geom = Some(gi);
                    }
                }
                _ => {
                    let flags;
                    let mut default_len = 0usize;
                    match field_type {
                        FieldType::String => {
                            let w = c.i32()?;
                            length = u32::try_from(w).map_err(|_| Error::Format("negative string width".into()))?;
                            flags = c.u8()?;
                            default_len = c.varuint32()? as usize;
                        }
                        FieldType::ObjectId | FieldType::Binary | FieldType::Guid | FieldType::GlobalId | FieldType::Xml => {
                            let w = c.u8()?;
                            length = u32::from(w);
                            flags = c.u8()?;
                        }
                        _ => {
                            let w = c.u8()?;
                            length = u32::from(w);
                            flags = c.u8()?;
                            default_len = c.u8()? as usize;
                        }
                    }
                    c.skip(default_len)?;
                    nullable = flags & 1 != 0;
                }
            }
            raster_kinds.push(raster_kind);
            fields.push(FieldInfo { name: fname, alias, field_type, nullable, length });
        }

        let n_nullable = fields.iter().filter(|f| f.nullable).count();
        let tablx_path = path.with_extension("gdbtablx");
        let tablx = if valid_count > 0 || tablx_path.exists() { Some(read_tablx(&tablx_path, version)?) } else { None };
        if valid_count > 0 && tablx.is_none() {
            return Err(Error::Unsupported("tables without .gdbtablx".into()));
        }
        let valid_count = tablx.as_ref().map_or(0, |t| valid_count.min(t.total));
        Ok(Self {
            name: name.to_string(),
            path: path.to_path_buf(),
            fields,
            raster_kinds,
            geom_index,
            geom,
            grid,
            strings_utf8,
            valid_count,
            nullable_bytes: n_nullable.div_ceil(8),
            tablx,
            data: OnceLock::new(),
        })
    }

    /// The table name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The path of the `.gdbtable` file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All fields in file order, including OBJECTID and the geometry field.
    pub fn fields(&self) -> &[FieldInfo] {
        &self.fields
    }

    /// Index of a field by name (case-insensitive).
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Geometry field description, for feature classes.
    pub fn geometry_info(&self) -> Option<&GeometryFieldInfo> {
        self.geom.as_ref()
    }

    /// Spatial index grid sizes of the geometry field (1 to 3 values; a single
    /// 0 means no spatial index). Empty for tables.
    pub fn spatial_index_grid(&self) -> &[f64] {
        &self.grid
    }

    /// Whether the table has a spatial index file (`.spx`).
    pub fn has_spatial_index_file(&self) -> bool {
        self.path.with_extension("spx").exists()
    }

    /// Index of the geometry field, for feature classes.
    pub fn geometry_field_index(&self) -> Option<usize> {
        self.geom_index
    }

    /// Number of non-deleted rows, as declared by the table header.
    pub fn feature_count(&self) -> u64 {
        self.valid_count
    }

    /// The largest OBJECTID slot of the table (including deleted rows).
    pub fn max_object_id(&self) -> u64 {
        self.tablx.as_ref().map_or(0, |t| t.total)
    }

    fn data(&self) -> Result<&[u8]> {
        let r = self.data.get_or_init(|| std::fs::read(&self.path).map_err(|e| format!("{}: {e}", self.path.display())));
        match r {
            Ok(d) => Ok(d),
            Err(e) => Err(Error::Format(format!("cannot read table: {e}"))),
        }
    }

    /// Reads all rows (OBJECTID order, deleted rows skipped).
    pub fn read_all(&self) -> Result<Vec<Row>> {
        self.rows()?.collect()
    }

    /// Iterates over all rows (OBJECTID order, deleted rows skipped).
    pub fn rows(&self) -> Result<RowIter<'_>> {
        self.rows_filtered(RowFilter::default())
    }

    /// Iterates over rows, optionally decoding only some columns and skipping
    /// rows whose geometry envelope misses a bounding box.
    pub fn rows_filtered(&self, filter: RowFilter) -> Result<RowIter<'_>> {
        let data = if self.valid_count > 0 { self.data()? } else { &[][..] };
        let prefix = self.tablx.as_ref().map(|t| t.prefix_blocks()).unwrap_or_default();
        let wanted = filter.columns.as_ref().map(|cols| {
            let mut w = vec![false; self.fields.len()];
            for &c in cols {
                if c < w.len() {
                    w[c] = true;
                }
            }
            w
        });
        Ok(RowIter { table: self, data, prefix, next_row: 0, wanted, bbox: filter.bbox })
    }

    /// Decodes the row blob for OBJECTID `oid`.
    fn decode_row(&self, oid: i64, blob: &[u8], wanted: Option<&[bool]>, bbox: Option<[f64; 4]>) -> Result<Option<Row>> {
        if blob.len() < self.nullable_bytes {
            return Err(Error::Format(format!("row {oid} shorter than its null bitmap")));
        }
        let nulls = &blob[..self.nullable_bytes];
        let mut c = Cursor::new(&blob[self.nullable_bytes..]);
        let mut values = Vec::with_capacity(self.fields.len());
        let mut i_null = 0usize;
        for (i, f) in self.fields.iter().enumerate() {
            if f.field_type == FieldType::ObjectId {
                values.push(Value::Int(oid));
                continue;
            }
            if f.nullable {
                let is_null = test_bit(nulls, i_null);
                i_null += 1;
                if is_null {
                    if bbox.is_some() && Some(i) == self.geom_index {
                        return Ok(None);
                    }
                    values.push(Value::Null);
                    continue;
                }
            }
            let want = wanted.is_none_or(|w| w[i]);
            let v = match f.field_type {
                FieldType::ObjectId => unreachable!(),
                FieldType::Int16 => Value::Int(i64::from(c.i16()?)),
                FieldType::Int32 => Value::Int(i64::from(c.i32()?)),
                FieldType::Int64 => Value::Int(c.i64()?),
                FieldType::Float32 => Value::Float32(c.f32()?),
                FieldType::Float64 => Value::Float64(c.f64()?),
                FieldType::DateTime | FieldType::Date | FieldType::Time => Value::DateTime(c.f64()?),
                FieldType::DateTimeOffset => {
                    let d = c.f64()?;
                    let off = c.i16()?;
                    Value::DateTime(d - f64::from(off) * 60.0 / 86400.0)
                }
                FieldType::String | FieldType::Xml => {
                    let n = c.varuint32()? as usize;
                    let b = c.bytes(n)?;
                    if !want {
                        Value::Null
                    } else if self.strings_utf8 || f.field_type != FieldType::String {
                        Value::String(String::from_utf8_lossy(b).into_owned())
                    } else {
                        Value::String(utf16(b))
                    }
                }
                FieldType::Guid | FieldType::GlobalId => {
                    let b = c.bytes(16)?;
                    Value::Guid(format!(
                        "{{{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
                    ))
                }
                FieldType::Binary => {
                    let n = c.varuint32()? as usize;
                    let b = c.bytes(n)?;
                    if want { Value::Binary(b.to_vec()) } else { Value::Null }
                }
                FieldType::Raster => {
                    if self.raster_kinds[i] == Some(RasterKind::Managed) {
                        Value::Int(i64::from(c.i32()?))
                    } else {
                        let n = c.varuint32()? as usize;
                        let b = c.bytes(n)?;
                        match self.raster_kinds[i] {
                            Some(RasterKind::External) => Value::String(utf16(b)),
                            _ => Value::Binary(b.to_vec()),
                        }
                    }
                }
                FieldType::Geometry => {
                    let n = c.varuint32()? as usize;
                    let b = c.bytes(n)?;
                    let g = self.geom.as_ref().expect("geometry field info");
                    if let Some(bb) = bbox {
                        match geometry::blob_extent(b, g)? {
                            Some(e) if e[0] <= bb[2] && e[2] >= bb[0] && e[1] <= bb[3] && e[3] >= bb[1] => {}
                            _ => return Ok(None),
                        }
                    }
                    if want {
                        match geometry::decode(b, g)? {
                            Some(s) => Value::Geometry(s),
                            None => Value::Null,
                        }
                    } else {
                        Value::Null
                    }
                }
            };
            values.push(if want { v } else { Value::Null });
        }
        Ok(Some(Row { object_id: oid, values }))
    }
}

/// Iterator over the rows of a [`Table`].
pub struct RowIter<'a> {
    table: &'a Table,
    data: &'a [u8],
    prefix: Vec<u32>,
    next_row: u64,
    wanted: Option<Vec<bool>>,
    bbox: Option<[f64; 4]>,
}

impl Iterator for RowIter<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let tablx = self.table.tablx.as_ref()?;
        while self.next_row < tablx.total {
            let i_row = self.next_row;
            self.next_row += 1;
            // Skip whole absent blocks quickly.
            if !tablx.block_map.is_empty() && i_row & 1023 == 0 && !test_bit(&tablx.block_map, (i_row / 1024) as usize) {
                self.next_row = i_row + 1024;
                continue;
            }
            let off = tablx.offset(i_row, &self.prefix);
            if off == 0 {
                continue;
            }
            let off = off as usize;
            let Some(len_bytes) = self.data.get(off..off + 4) else {
                return Some(Err(Error::Format(format!("row {} offset beyond end of file", i_row + 1))));
            };
            let len = u32::from_le_bytes(len_bytes.try_into().expect("4"));
            if len >> 31 != 0 {
                // Deleted row whose index entry is out of sync.
                continue;
            }
            let Some(blob) = self.data.get(off + 4..off + 4 + len as usize) else {
                return Some(Err(Error::Format(format!("row {} extends beyond end of file", i_row + 1))));
            };
            match self.table.decode_row(i_row as i64 + 1, blob, self.wanted.as_deref(), self.bbox) {
                Ok(Some(r)) => return Some(Ok(r)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}
