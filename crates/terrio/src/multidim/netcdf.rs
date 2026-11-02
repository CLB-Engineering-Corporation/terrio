//! Metadata and typed hyperslabs shared by netCDF classic, netCDF-4 and numeric HDF5.
use super::*;
use netcdf_reader::{NcAttrValue, NcAttribute, NcFile, NcGroup, NcMetadataMode, NcOpenOptions, NcSliceInfo, NcSliceInfoElem, NcType};

fn error(e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("multidim: {e}"))
}
fn numeric_type(t: &NcType) -> Option<DataType> {
    Some(match t {
        NcType::Byte => DataType::I8,
        NcType::UByte => DataType::U8,
        NcType::Short => DataType::I16,
        NcType::UShort => DataType::U16,
        NcType::Int => DataType::I32,
        NcType::UInt => DataType::U32,
        NcType::Int64 => DataType::I64,
        NcType::UInt64 => DataType::U64,
        NcType::Float => DataType::F32,
        NcType::Double => DataType::F64,
        _ => return None,
    })
}
fn floating(value: f64) -> Value {
    if value.is_nan() {
        Value::String("NaN".into())
    } else if value == f64::INFINITY {
        Value::String("Infinity".into())
    } else if value == f64::NEG_INFINITY {
        Value::String("-Infinity".into())
    } else {
        serde_json::json!(value)
    }
}
fn attribute(a: &NcAttrValue) -> Value {
    fn one(v: Vec<Value>) -> Value {
        if v.len() == 1 { v.into_iter().next().unwrap() } else { Value::Array(v) }
    }
    macro_rules! values {
        ($v:expr) => {
            one($v.iter().map(|x| serde_json::json!(x)).collect())
        };
    }
    match a {
        NcAttrValue::Bytes(v) => values!(v),
        NcAttrValue::UBytes(v) => values!(v),
        NcAttrValue::Shorts(v) => values!(v),
        NcAttrValue::UShorts(v) => values!(v),
        NcAttrValue::Ints(v) => values!(v),
        NcAttrValue::UInts(v) => values!(v),
        NcAttrValue::Int64s(v) => values!(v),
        NcAttrValue::UInt64s(v) => values!(v),
        NcAttrValue::Floats(v) => one(v.iter().map(|&x| floating(x as f64)).collect()),
        NcAttrValue::Doubles(v) => one(v.iter().map(|&x| floating(x)).collect()),
        NcAttrValue::Chars(s) => Value::String(s.clone()),
        NcAttrValue::Strings(v) => one(v.iter().cloned().map(Value::String).collect()),
    }
}
fn attributes(attrs: &[NcAttribute]) -> Value {
    Value::Object(attrs.iter().map(|a| (a.name.clone(), attribute(&a.value))).collect())
}
fn attribute_types(attrs: &[NcAttribute]) -> AttributeTypes {
    attrs
        .iter()
        .filter_map(|a| {
            let dtype = match &a.value {
                NcAttrValue::Bytes(_) => DataType::I8,
                NcAttrValue::UBytes(_) => DataType::U8,
                NcAttrValue::Shorts(_) => DataType::I16,
                NcAttrValue::UShorts(_) => DataType::U16,
                NcAttrValue::Ints(_) => DataType::I32,
                NcAttrValue::UInts(_) => DataType::U32,
                NcAttrValue::Int64s(_) => DataType::I64,
                NcAttrValue::UInt64s(_) => DataType::U64,
                NcAttrValue::Floats(_) => DataType::F32,
                NcAttrValue::Doubles(_) => DataType::F64,
                NcAttrValue::Chars(_) | NcAttrValue::Strings(_) => return None,
            };
            Some((a.name.clone(), dtype))
        })
        .collect()
}
fn group(g: &NcGroup, p: &str, file: &Arc<NcFile>, hdf: Option<&hdf5_reader::Hdf5File>, inherited: &[Dimension]) -> Result<Group> {
    let mut out = Group::new(p);
    out.attributes = attributes(&g.attributes);
    out.attribute_types = attribute_types(&g.attributes);
    let mut visible = inherited.to_vec();
    for d in &g.dimensions {
        let full = path(p, &d.name);
        let local_scale = hdf
            .and_then(|h| h.dataset(&full).ok())
            .and_then(|v| v.attribute("CLASS").ok())
            .and_then(|a| a.read_string().ok())
            .is_some_and(|c| c == "DIMENSION_SCALE");
        if !local_scale && inherited.iter().any(|i| i.name == d.name && i.size as u64 == d.size) {
            continue;
        }
        let size = usize::try_from(d.size).map_err(error)?;
        let dimension = Dimension {
            name: d.name.clone(),
            path: full.clone(),
            size,
            unlimited: d.is_unlimited,
            indexing_variable: g.variables.iter().any(|v| v.name == d.name && v.is_coordinate_variable()).then_some(full),
        };
        visible.retain(|i| i.name != d.name);
        visible.push(dimension.clone());
        out.dimensions.push(dimension);
    }
    for v in &g.variables {
        let full = path(p, &v.name);
        let Some(data_type) =
            (if v.dtype == NcType::Char && v.dimensions.is_empty() { Some(DataType::U8) } else { numeric_type(&v.dtype) })
        else {
            out.unsupported_arrays.push(UnsupportedArray {
                path: full,
                data_type: format!("{:?}", v.dtype),
                shape: v.shape().into_iter().map(|n| usize::try_from(n).map_err(error)).collect::<Result<_>>()?,
            });
            continue;
        };
        let mut dimensions = Vec::new();
        for (i, d) in v.dimensions.iter().enumerate() {
            let size = usize::try_from(d.size).map_err(error)?;
            let dimension = match visible.iter().rev().find(|n| n.name == d.name && n.size == size) {
                Some(d) => d.clone(),
                None => {
                    let name = format!("{}_dim{i}", v.name);
                    let dimension = Dimension { path: path(p, &name), name, size, unlimited: d.is_unlimited, indexing_variable: None };
                    out.dimensions.push(dimension.clone());
                    dimension
                }
            };
            dimensions.push(dimension);
        }
        let chunks = hdf.and_then(|h| h.dataset(&full).ok()).and_then(|d| d.chunks()).map(|s| s.into_iter().map(|n| n as usize).collect());
        let coordinates = dimensions.iter().filter_map(|d| d.indexing_variable.clone()).collect();
        out.arrays.push(Array {
            info: ArrayInfo {
                name: v.name.clone(),
                path: full,
                data_type,
                dimensions,
                chunks,
                attributes: attributes(&v.attributes),
                coordinates,
            },
            source: Source::Netcdf(Arc::clone(file)),
            attribute_types: attribute_types(&v.attributes),
        });
    }
    // Lossy netCDF reconstruction can omit HDF5-specific datatypes. Report those
    // datasets explicitly rather than making numeric siblings hide their existence.
    if let Some(hdf) = hdf {
        let mut raw_group = hdf.root_group().map_err(error)?;
        for name in p.split('/').filter(|n| !n.is_empty()) {
            raw_group = raw_group.group(name).map_err(error)?;
        }
        for raw in raw_group.datasets().map_err(error)? {
            let name = raw.name().rsplit('/').next().unwrap_or(raw.name());
            if g.variables.iter().any(|v| v.name == name) {
                continue;
            }
            let dimension_only = raw
                .attribute("NAME")
                .ok()
                .and_then(|a| a.read_string().ok())
                .is_some_and(|name| name.starts_with("This is a netCDF dimension but not a netCDF variable."));
            if dimension_only {
                continue;
            }
            out.unsupported_arrays.push(UnsupportedArray {
                path: path(p, name),
                data_type: format!("{:?}", raw.dtype()),
                shape: raw.shape().iter().map(|&n| usize::try_from(n).map_err(error)).collect::<Result<_>>()?,
            });
        }
    }
    for child in &g.groups {
        out.groups.push(group(child, &path(p, &child.name), file, hdf, &visible)?);
    }
    Ok(out)
}
fn resolve_coordinates(root: &mut Group) {
    let names: std::collections::HashSet<_> = root.arrays_recursive().iter().map(|a| a.info.path.clone()).collect();
    fn visit(group: &mut Group, names: &std::collections::HashSet<String>) {
        for a in &mut group.arrays {
            if let Some(raw) = a.info.attributes.get("coordinates").and_then(Value::as_str) {
                for name in raw.split_whitespace() {
                    let mut parent = group.path.as_str();
                    let coordinate = loop {
                        let candidate = if name.starts_with('/') { name.to_owned() } else { path(parent, name) };
                        if names.contains(&candidate) {
                            break Some(candidate);
                        }
                        if name.starts_with('/') || parent == "/" {
                            break None;
                        }
                        parent = parent.rsplit_once('/').map(|(p, _)| if p.is_empty() { "/" } else { p }).unwrap_or("/");
                    };
                    if let Some(c) = coordinate
                        && !a.info.coordinates.contains(&c)
                    {
                        a.info.coordinates.push(c);
                    }
                }
            }
        }
        for child in &mut group.groups {
            visit(child, names);
        }
    }
    visit(root, &names);
}
fn dataset(file: NcFile, hdf: Option<hdf5_reader::Hdf5File>, driver: &str) -> Result<Dataset> {
    let file = Arc::new(file);
    let mut root = group(file.root_group().map_err(error)?, "/", &file, hdf.as_ref(), &[])?;
    resolve_coordinates(&mut root);
    Ok(Dataset { driver: driver.into(), root })
}
struct OffsetStorage {
    inner: hdf5_reader::storage::DynStorage,
    offset: u64,
}
impl hdf5_reader::storage::Storage for OffsetStorage {
    fn len(&self) -> u64 {
        self.inner.len() - self.offset
    }
    fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<hdf5_reader::storage::StorageBuffer> {
        self.inner.read_range(self.offset.checked_add(offset).ok_or(hdf5_reader::error::Error::OffsetOutOfBounds(offset))?, len)
    }
}
pub(super) fn open(name: &str) -> Result<Dataset> {
    let mut storage: hdf5_reader::storage::DynStorage = Arc::new(hdf5_reader::storage::FileStorage::open(name).map_err(error)?);
    let header = storage.read_range(0, storage.len().min(8) as usize).map_err(error)?;
    let classic = header.starts_with(b"CDF");
    if !classic {
        let mut offset = 0u64;
        for _ in 0..20 {
            if offset.checked_add(8).is_none_or(|end| end > storage.len()) {
                break;
            }
            if storage.read_range(offset, 8).map_err(error)?.as_ref() == b"\x89HDF\r\n\x1a\n" {
                if offset > 0 {
                    storage = Arc::new(OffsetStorage { inner: storage, offset });
                }
                break;
            }
            offset = if offset == 0 { 512 } else { offset * 2 };
        }
    }
    let file = NcFile::from_storage_with_options(
        Arc::clone(&storage),
        NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() },
    )
    .map_err(error)?;
    let hdf = (!classic).then(|| hdf5_reader::Hdf5File::from_storage(storage).ok()).flatten();
    let driver = if crate::drivers::netcdf::is_netcdf(std::path::Path::new(name)) { "netCDF" } else { "HDF5" };
    dataset(file, hdf, driver)
}
pub(super) fn open_bytes(bytes: &[u8]) -> Result<Dataset> {
    let body = crate::drivers::hdf5::signature_offset_in(bytes).map(|offset| &bytes[offset..]).unwrap_or(bytes);
    open_bytes_owned(body.to_vec())
}
fn owned_storage(bytes: Vec<u8>) -> hdf5_reader::storage::DynStorage {
    let offset = crate::drivers::hdf5::signature_offset_in(&bytes).unwrap_or(0);
    let storage: hdf5_reader::storage::DynStorage = Arc::new(hdf5_reader::storage::BytesStorage::new(bytes));
    if offset == 0 { storage } else { Arc::new(OffsetStorage { inner: storage, offset: offset as u64 }) }
}
pub(super) fn open_bytes_owned(bytes: Vec<u8>) -> Result<Dataset> {
    let classic = bytes.starts_with(b"CDF");
    // Both metadata readers share retained storage, including any userblock.
    // The pinned BytesStorage converts Vec to Arc<[u8]>; avoid an extra Vec clone here.
    let storage = owned_storage(bytes);
    let file = NcFile::from_storage_with_options(
        Arc::clone(&storage),
        NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() },
    )
    .map_err(error)?;
    let hdf = (!classic).then(|| hdf5_reader::Hdf5File::from_storage(storage).ok()).flatten();
    dataset(file, hdf, if classic { "netCDF" } else { "netCDF/HDF5" })
}
pub(super) fn read(file: &NcFile, name: &str, t: DataType, selections: &[Selection]) -> Result<Vec<u8>> {
    let variable = file.variable(name.trim_start_matches('/')).map_err(error)?;
    if variable.dtype == NcType::Char && variable.dimensions.is_empty() {
        return file.read_variable_native_bytes(name.trim_start_matches('/')).map_err(error);
    }
    let selection = NcSliceInfo {
        selections: selections
            .iter()
            .map(|s| NcSliceInfoElem::Slice {
                start: s.start as u64,
                end: (s.start + (s.count - 1) * s.step + 1) as u64,
                step: s.step as u64,
            })
            .collect(),
    };
    macro_rules! read {
        ($t:ty) => {{
            let array = file.read_variable_slice::<$t>(name.trim_start_matches('/'), &selection).map_err(error)?;
            array.iter().flat_map(|v| v.to_le_bytes()).collect()
        }};
    }
    Ok(match t {
        DataType::U8 => read!(u8),
        DataType::I8 => read!(i8),
        DataType::U16 => read!(u16),
        DataType::I16 => read!(i16),
        DataType::U32 => read!(u32),
        DataType::I32 => read!(i32),
        DataType::U64 => read!(u64),
        DataType::I64 => read!(i64),
        DataType::F32 => read!(f32),
        DataType::F64 => read!(f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdf5_reader::storage::{BytesStorage, Storage, StorageBuffer};
    use std::sync::atomic::{AtomicU64, Ordering};
    fn classic_fixture() -> Vec<u8> {
        let mut bytes = b"CDF\x01".to_vec();
        // CDF-1: one dimension, no attributes, one packed Int16 variable.
        for word in [0u32, 10, 1, 1, 0x78000000, 3, 0, 0, 11, 1, 1, 0x76000000, 1, 0, 0, 0, 3, 8, 80] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        assert_eq!(bytes.len(), 80);
        for value in [-7i16, 0, 42, 0] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes
    }
    #[test]
    fn owned_classic_preserves_values_and_retained_views() {
        let bytes = classic_fixture();
        let borrowed = super::open_bytes(&bytes).unwrap();
        let owned = crate::multidim::open_bytes_owned(bytes).unwrap();
        let array = owned.array("v").unwrap();
        assert_eq!(serde_json::to_value(&array.info).unwrap(), serde_json::to_value(&borrowed.array("v").unwrap().info).unwrap());
        let view = array.view();
        drop(array);
        drop(owned);
        assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
    }
    #[test]
    fn owned_hdf_userblocks_share_retained_storage_and_retain_views() {
        use crate::drivers::hdf5_write::{self, H5Type};
        let raw = hdf5_write::encode(&hdf5_write::File {
            datasets: vec![hdf5_write::Dataset {
                name: "sample".into(),
                dtype: H5Type::Int { size: 2, signed: true },
                shape: Some(vec![3]),
                max_shape: None,
                data: [-7i16, 0, 42].into_iter().flat_map(i16::to_le_bytes).collect(),
                fill: vec![0; 2],
                storage: Default::default(),
                attrs: vec![],
            }],
            ..Default::default()
        });
        for offset in [0, 512, 1024, 4096] {
            let mut bytes = vec![0; offset];
            bytes.extend_from_slice(&raw);
            let size = bytes.len();
            let storage = owned_storage(bytes.clone());
            assert_eq!(storage.len() as usize, raw.len());
            assert!(storage.read_range(u64::MAX, 8).is_err());
            assert!(storage.read_range(raw.len() as u64, 1).is_err());
            let storage = owned_storage(bytes);
            let header = storage.read_range(0, 8).unwrap();
            let again = storage.read_range(0, 8).unwrap();
            assert_eq!(header.as_ptr(), again.as_ptr());
            assert_eq!(header.as_ref(), &raw[..8]);
            eprintln!(
                "owned storage identity: input={size}, userblock={offset}, payload={}, explicit Vec clone bytes=0; dependency Vec-to-Arc copy remains",
                raw.len()
            );
            let mut bytes = vec![0; offset];
            bytes.extend_from_slice(&raw);
            let borrowed = super::open_bytes(&bytes).unwrap();
            let owned = crate::multidim::open_bytes_owned(bytes).unwrap();
            let array = owned.array("sample").unwrap();
            assert_eq!(serde_json::to_value(&array.info).unwrap(), serde_json::to_value(&borrowed.array("sample").unwrap().info).unwrap());
            let view = array.view();
            drop(array);
            drop(owned);
            assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
        }
    }
    #[test]
    fn owned_producer_written_hdf_userblock() {
        // h5py.File(path, 'w', userblock_size=512), create_dataset('sample',
        // data=[-7, 0, 42], dtype='<i2'): the producer writes real base addresses.
        let bytes = include_bytes!("../../tests/data/multidim-userblock.h5");
        assert_eq!(crate::drivers::hdf5::signature_offset_in(bytes), Some(512));
        for ds in [super::open_bytes(bytes).unwrap(), crate::multidim::open_bytes_owned(bytes.to_vec()).unwrap()] {
            let view = ds.array("sample").unwrap().view();
            drop(ds);
            assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
        }
    }
    #[test]
    fn owned_malformed_inputs_return_errors() {
        for bytes in [vec![], b"CDF".to_vec(), b"not scientific data".to_vec(), b"\x89HDF\r\n\x1a\n".to_vec(), vec![0; 1024]] {
            assert!(crate::multidim::open_bytes_owned(bytes).is_err());
        }
        let mut bytes = classic_fixture();
        bytes.truncate(24);
        assert!(crate::multidim::open_bytes_owned(bytes).is_err());
    }
    struct CountingStorage {
        inner: BytesStorage,
        bytes: Arc<AtomicU64>,
    }
    impl Storage for CountingStorage {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_range(&self, offset: u64, length: usize) -> hdf5_reader::error::Result<StorageBuffer> {
            self.bytes.fetch_add(length as u64, Ordering::Relaxed);
            self.inner.read_range(offset, length)
        }
    }
    #[test]
    fn selected_reads_request_a_fraction_of_the_array_bytes() {
        let dimensions: Vec<_> = [("time", 64), ("depth", 2), ("y", 64), ("x", 64)]
            .into_iter()
            .map(|(name, size)| Dimension { name: name.into(), path: format!("/{name}"), size, unlimited: false, indexing_variable: None })
            .collect();
        let shape: Vec<_> = dimensions.iter().map(|d| d.size).collect();
        let count = elements(&shape).unwrap();
        let array = Array::from_bytes(
            ArrayInfo {
                name: "cube".into(),
                path: "/cube".into(),
                data_type: DataType::I64,
                dimensions: dimensions.clone(),
                chunks: Some(vec![1, 1, 16, 16]),
                attributes: serde_json::json!({}),
                coordinates: vec![],
            },
            (0..count as i64).flat_map(i64::to_le_bytes).collect(),
        )
        .unwrap();
        let mut root = Group::new("/");
        root.dimensions = dimensions;
        root.arrays.push(array);
        let raw = Dataset { driver: "Memory".into(), root }.to_netcdf_bytes().unwrap();
        let size = raw.len();
        let transferred = Arc::new(AtomicU64::new(0));
        let storage = Arc::new(CountingStorage { inner: BytesStorage::new(raw), bytes: Arc::clone(&transferred) });
        let file = NcFile::from_storage_with_options(storage, NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() })
            .unwrap();
        let dataset = super::dataset(file, None, "netCDF").unwrap();
        let baseline = transferred.load(Ordering::Relaxed);
        let array = dataset.array("cube").unwrap();
        let slab = array
            .view()
            .isel("time", 30)
            .unwrap()
            .isel("depth", 1)
            .unwrap()
            .slice("y", 0, 16, 1)
            .unwrap()
            .slice("x", 0, 16, 1)
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(slab.shape, [16, 16]);
        assert_eq!(slab.values().unwrap()[0], serde_json::json!(249856));
        let selected = transferred.load(Ordering::Relaxed) - baseline;
        array.read().unwrap();
        let full = transferred.load(Ordering::Relaxed) - baseline - selected;
        eprintln!("range bytes: metadata={baseline}, selected={selected}, whole={full}, file={size}");
        assert!(selected < (size / 8) as u64, "selected read fetched {selected} bytes from a {size}-byte file");
        assert!(full > selected * 4, "full read {full} was not substantially larger than selected read {selected}");
    }
}

#[cfg(test)]
mod attribute_tests {
    use super::*;
    use crate::drivers::hdf5_write::{self, Attr, H5Type, Values};
    #[test]
    fn numeric_attribute_types_and_nonfinite_values_survive_snapshots() {
        let numeric = |name: &str, values: &[f32]| Attr {
            name: name.into(),
            dtype: H5Type::Float { size: 4 },
            shape: (values.len() != 1).then(|| vec![values.len() as u64]),
            values: Values::Bytes(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
        };
        let file = hdf5_write::File {
            attrs: vec![
                numeric("nan32", &[f32::NAN]),
                numeric("vector32", &[f32::INFINITY, f32::NEG_INFINITY, 1.5]),
                Attr::text("literal", "NaN"),
            ],
            datasets: vec![hdf5_write::Dataset {
                name: "sample".into(),
                dtype: H5Type::Int { size: 1, signed: false },
                shape: Some(vec![2]),
                max_shape: None,
                data: vec![1, 2],
                fill: vec![0],
                storage: Default::default(),
                attrs: vec![numeric("minimum32", &[f32::NEG_INFINITY])],
            }],
            ..Default::default()
        };
        let original = crate::multidim::open_bytes(&hdf5_write::encode(&file)).unwrap();
        let check = |ds: &Dataset| {
            assert_eq!(ds.root.attribute_type("nan32"), Some(DataType::F32));
            assert_eq!(ds.root.attribute_type("vector32"), Some(DataType::F32));
            assert_eq!(ds.root.attribute_type("literal"), None);
            assert_eq!(ds.root.attributes["nan32"], "NaN");
            assert_eq!(ds.root.attributes["literal"], "NaN");
            assert_eq!(ds.root.attributes["vector32"], serde_json::json!(["Infinity", "-Infinity", 1.5]));
            assert_eq!(ds.array("sample").unwrap().attribute_type("minimum32"), Some(DataType::F32));
        };
        check(&original);
        check(&crate::multidim::open_bytes(&original.to_netcdf_bytes().unwrap()).unwrap());
        #[cfg(feature = "zarr")]
        {
            let zarr = crate::multidim::open_zarr_store(original.to_zarr_store().unwrap()).unwrap();
            check(&zarr);
            assert!(zarr.root.attributes.get("_terrio_attribute_types").is_none());
            check(&crate::multidim::open_bytes(&zarr.to_netcdf_bytes().unwrap()).unwrap());
        }
    }
}
