//! Zstandard TIFF chunks without the zstd C library: the `tiff` crate decodes Zstandard with its
//! `zstd` feature only (the `libzstd` features of terrio and terrio-crs, natively). Without it, and
//! in WebAssembly, a chunk is decompressed with `ruzstd` and handed to the crate as the only chunk
//! of an uncompressed TIFF with the image's sample layout: the crate then unpacks, converts and
//! inverts its samples (bit depths under 8, Float16, WhiteIsZero, predictors) exactly as it does
//! with the C library.

use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use std::io::{Cursor, Error, ErrorKind, Read, Seek, SeekFrom};
use tiff::decoder::{ChunkType, Decoder, DecodingResult, Limits};
use tiff::tags::{ByteOrder, Tag};
use tiff::{TiffError, TiffResult};

/// TIFF's Zstandard compression.
pub const ZSTD: u16 = 50000;

/// TIFF field types: 16 and 32-bit unsigned integers.
const SHORT: u16 = 3;
const LONG: u16 = 4;

/// The first `max` bytes `data` decompresses to with `ruzstd`, frame after frame (its decoder
/// reads one), skippable frames skipped.
pub fn frames(mut data: &[u8], max: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while !data.is_empty() && out.len() < max {
        match ruzstd::decoding::StreamingDecoder::new(&mut data) {
            Ok(frame) => {
                frame.take((max - out.len()) as u64).read_to_end(&mut out)?;
            }
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                data = data
                    .get(length as usize..)
                    .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "Zstandard: truncated skippable frame"))?;
            }
            Err(e) => return Err(Error::new(ErrorKind::InvalidData, format!("Zstandard: {e}"))),
        }
    }
    Ok(out)
}

/// Whether this build's `tiff` crate decodes Zstandard itself (with the zstd C library).
pub const fn crate_decodes_zstd() -> bool {
    cfg!(all(feature = "libzstd", not(target_family = "wasm")))
}

/// Decodes chunk `idx` of the image the decoder is on, as `Decoder::read_chunk` does, a Zstandard
/// chunk without the C library when the crate was built without it.
pub fn read_chunk<R: Read + Seek>(dec: &mut Decoder<R>, idx: u32, limit: usize) -> TiffResult<DecodingResult> {
    if !crate_decodes_zstd() && dec.find_tag_unsigned::<u16>(Tag::Compression)? == Some(ZSTD) {
        read_zstd_chunk(dec, idx, limit)
    } else {
        dec.read_chunk(idx)
    }
}

/// Decodes the Zstandard chunk `idx` of the image the decoder is on into the samples
/// `Decoder::read_chunk` gives with the zstd C library, without it. A chunk decompressing to more
/// than `limit` bytes is refused, before any of it is decompressed, and a stream expanding past its
/// chunk is decompressed no further than the chunk.
pub fn read_zstd_chunk<R: Read + Seek>(dec: &mut Decoder<R>, idx: u32, limit: usize) -> TiffResult<DecodingResult> {
    // A sample layout the crate does not decode is refused as it refuses it.
    dec.colortype()?;
    let tiled = dec.get_chunk_type() == ChunkType::Tile;
    let (offsets, counts) = if tiled { (Tag::TileOffsets, Tag::TileByteCounts) } else { (Tag::StripOffsets, Tag::StripByteCounts) };
    let (offsets, counts) = (dec.get_tag_u64_vec(offsets)?, dec.get_tag_u64_vec(counts)?);
    let (Some(&offset), Some(&count)) = (offsets.get(idx as usize), counts.get(idx as usize)) else {
        return Err(invalid(format!("no TIFF chunk {idx}")));
    };
    let (cw, ch) = dec.chunk_dimensions();
    let (dw, dh) = dec.chunk_data_dimensions(idx);
    let mut u16s = |tag: Tag| dec.find_tag_unsigned_vec::<u16>(tag);
    let samples = u16s(Tag::SamplesPerPixel)?.and_then(|v| v.first().copied()).unwrap_or(1).max(1);
    let planar = samples > 1 && u16s(Tag::PlanarConfiguration)?.and_then(|v| v.first().copied()) == Some(2);
    // A chunk of a planar image holds one sample of each pixel.
    let spp = if planar { 1 } else { samples };
    let per_sample = |v: Option<Vec<u16>>, default: u16| {
        let mut v = v.unwrap_or_default();
        v.resize(usize::from(spp), v.first().copied().unwrap_or(default));
        v.into_iter().map(u32::from).collect::<Vec<u32>>()
    };
    let photometric = u16s(Tag::PhotometricInterpretation)?.and_then(|v| v.first().copied()).unwrap_or(1);
    // The crate inverts WhiteIsZero images of one sample only.
    if planar && photometric == 0 {
        return Err(invalid("WhiteIsZero TIFF images of several samples are not supported"));
    }
    let bits = per_sample(u16s(Tag::BitsPerSample)?, 1);
    // The bytes of the chunk: a tile is stored whole, a strip with its own rows, each row ending
    // on a byte. No more is decompressed, whatever the stream expands to.
    let (w, h) = if tiled { (cw, ch) } else { (dw, dh) };
    let row_bits = u64::from(w).saturating_mul(bits.iter().map(|b| u64::from(*b)).sum());
    let chunk_bytes = usize::try_from(row_bits.div_ceil(8).saturating_mul(u64::from(h))).unwrap_or(usize::MAX);
    let mut fields: Vec<(u16, u16, Vec<u32>)> = vec![
        (256, LONG, vec![dw]),
        (257, LONG, vec![dh]),
        (258, SHORT, bits),
        (259, SHORT, vec![1]),
        // One plane of a color image is a gray one.
        (262, SHORT, vec![u32::from(if planar { 1 } else { photometric })]),
        (277, SHORT, vec![u32::from(spp)]),
        (284, SHORT, vec![1]),
        (339, SHORT, per_sample(u16s(Tag::SampleFormat)?, 1)),
    ];
    // The tags that change how the crate lays samples out, as the file has them.
    let mut copied = vec![Tag::FillOrder, Tag::Predictor];
    if !planar {
        copied.extend([Tag::ColorMap, Tag::ExtraSamples, Tag::Unknown(530)]);
    }
    for tag in copied {
        if let Some(v) = u16s(tag)? {
            fields.push((tag.to_u16(), SHORT, v.into_iter().map(u32::from).collect()));
        }
    }
    if tiled {
        fields.extend([(322, LONG, vec![cw]), (323, LONG, vec![ch])]);
    } else {
        fields.push((278, LONG, vec![dh.max(1)]));
    }
    let big_endian = dec.byte_order() == ByteOrder::BigEndian;
    let r = dec.inner();
    r.seek(SeekFrom::Start(offset))?;
    // The buffer grows as bytes arrive: a corrupt byte count allocates no more than the file holds.
    let mut raw = Vec::new();
    r.take(count).read_to_end(&mut raw)?;
    if raw.len() as u64 != count {
        return Err(invalid(format!("TIFF chunk {idx}: {} bytes, {count} expected", raw.len())));
    }
    if chunk_bytes > limit {
        return Err(TiffError::LimitsExceeded);
    }
    let data = frames(&raw, chunk_bytes)?;
    drop(raw);
    let file = tiff_file(big_endian, fields, tiled, &[&data])?;
    drop(data);
    let mut limits = Limits::default();
    limits.decoding_buffer_size = limit;
    limits.intermediate_buffer_size = limit;
    Decoder::new(Cursor::new(file))?.with_limits(limits).read_chunk(0)
}

/// A classic TIFF of one image with `fields` (tag, field type, values) and its `chunks`, tiles
/// or strips, after the directory.
fn tiff_file(big_endian: bool, mut fields: Vec<(u16, u16, Vec<u32>)>, tiled: bool, chunks: &[&[u8]]) -> TiffResult<Vec<u8>> {
    let too_large = || invalid("TIFF chunk too large");
    let size = |kind: u16, n: usize| n * if kind == SHORT { 2 } else { 4 };
    let (offsets_tag, counts_tag) = if tiled { (324, 325) } else { (273, 279) };
    // Placeholders, for the directory's size: the offsets are known once it is.
    fields.push((offsets_tag, LONG, vec![0; chunks.len()]));
    fields.push((counts_tag, LONG, vec![0; chunks.len()]));
    fields.sort_by_key(|f| f.0);
    // The header, the directory (a count, 12 bytes per field, the next directory's offset), then
    // the values that do not fit a field's 4 bytes, each on an even offset.
    let directory_end = 8 + 2 + 12 * fields.len() + 4;
    let values: usize = fields.iter().map(|(_, kind, v)| size(*kind, v.len())).filter(|n| *n > 4).map(|n| n + n % 2).sum();
    let mut at = directory_end + values;
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut counts = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        offsets.push(u32::try_from(at).map_err(|_| too_large())?);
        counts.push(u32::try_from(chunk.len()).map_err(|_| too_large())?);
        at = at.checked_add(chunk.len()).ok_or_else(too_large)?;
    }
    u32::try_from(at).map_err(|_| too_large())?;
    for field in &mut fields {
        if field.0 == offsets_tag {
            field.2 = std::mem::take(&mut offsets);
        } else if field.0 == counts_tag {
            field.2 = std::mem::take(&mut counts);
        }
    }
    let mut out = Vec::with_capacity(at);
    let u16_bytes = |v: u16| if big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
    let u32_bytes = |v: u32| if big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
    let value_bytes = |kind: u16, values: &[u32]| -> Vec<u8> {
        values.iter().flat_map(|&v| if kind == SHORT { u16_bytes(v as u16).to_vec() } else { u32_bytes(v).to_vec() }).collect()
    };
    out.extend_from_slice(if big_endian { b"MM" } else { b"II" });
    out.extend_from_slice(&u16_bytes(42));
    out.extend_from_slice(&u32_bytes(8));
    out.extend_from_slice(&u16_bytes(fields.len() as u16));
    let mut long_values = Vec::with_capacity(values);
    for (tag, kind, v) in &fields {
        out.extend_from_slice(&u16_bytes(*tag));
        out.extend_from_slice(&u16_bytes(*kind));
        out.extend_from_slice(&u32_bytes(v.len() as u32));
        let mut bytes = value_bytes(*kind, v);
        if bytes.len() > 4 {
            out.extend_from_slice(&u32_bytes((directory_end + long_values.len()) as u32));
            bytes.resize(bytes.len() + bytes.len() % 2, 0);
            long_values.extend_from_slice(&bytes);
        } else {
            bytes.resize(4, 0);
            out.extend_from_slice(&bytes);
        }
    }
    out.extend_from_slice(&u32_bytes(0));
    out.extend_from_slice(&long_values);
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    Ok(out)
}

/// An error with a message, as the `tiff` crate reports I/O errors.
fn invalid(msg: impl Into<String>) -> TiffError {
    TiffError::IoError(Error::new(ErrorKind::InvalidData, msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample layout: bits, sample format, samples per pixel, photometric interpretation,
    /// predictor and whether the samples are in separate planes.
    #[derive(Debug, Clone, Copy)]
    struct Layout {
        bits: u32,
        format: u32,
        samples: u32,
        photometric: u32,
        predictor: u32,
        planar: bool,
    }

    const fn layout(bits: u32, format: u32, samples: u32, photometric: u32, predictor: u32, planar: bool) -> Layout {
        Layout { bits, format, samples, photometric, predictor, planar }
    }

    /// The layouts the `tiff` crate decodes that terrio's own Zstandard path leaves to it (bit
    /// depths under 8, Float16, WhiteIsZero), and ordinary ones.
    const LAYOUTS: &[Layout] = &[
        layout(1, 1, 1, 1, 1, false),
        layout(1, 1, 1, 0, 1, false),
        layout(2, 1, 1, 1, 1, false),
        layout(4, 1, 1, 3, 1, false),
        layout(4, 1, 1, 0, 1, false),
        layout(8, 1, 1, 0, 1, false),
        layout(8, 1, 1, 0, 2, false),
        layout(16, 1, 1, 0, 2, false),
        layout(16, 3, 1, 1, 1, false),
        layout(16, 3, 1, 1, 3, false),
        layout(16, 3, 3, 2, 3, false),
        layout(32, 3, 1, 1, 3, false),
        layout(64, 3, 1, 1, 1, false),
        layout(16, 2, 1, 1, 2, false),
        layout(8, 1, 3, 2, 2, false),
        layout(8, 1, 3, 2, 1, true),
        layout(16, 1, 2, 0, 2, true),
        layout(16, 3, 3, 2, 3, true),
    ];

    const WIDTH: u32 = 37;
    const HEIGHT: u32 = 23;

    /// Deterministic bytes that compress a little.
    fn bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..n)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if i % 5 < 2 { (i / 7) as u8 } else { state as u8 }
            })
            .collect()
    }

    /// A Zstandard TIFF of `layout`, in 16 x 16 tiles or strips of 5 rows, with its chunks'
    /// uncompressed bytes.
    fn zstd_tiff(l: Layout, tiled: bool, big_endian: bool) -> (Vec<u8>, Vec<Vec<u8>>) {
        let spp = if l.planar { 1 } else { l.samples };
        let per_sample = |v: u32| vec![v; l.samples as usize];
        let mut fields = vec![
            (256, LONG, vec![WIDTH]),
            (257, LONG, vec![HEIGHT]),
            (258, SHORT, per_sample(l.bits)),
            (259, SHORT, vec![u32::from(ZSTD)]),
            (262, SHORT, vec![l.photometric]),
            (277, SHORT, vec![l.samples]),
            (284, SHORT, vec![if l.planar { 2 } else { 1 }]),
            (317, SHORT, vec![l.predictor]),
            (339, SHORT, per_sample(l.format)),
        ];
        if l.photometric == 3 {
            fields.push((320, SHORT, (0..3 * (1u32 << l.bits)).map(|i| i * 257 % 65536).collect()));
        }
        let sizes: Vec<(u32, u32)> = if tiled {
            fields.extend([(322, LONG, vec![16]), (323, LONG, vec![16])]);
            vec![(16, 16); (WIDTH.div_ceil(16) * HEIGHT.div_ceil(16)) as usize]
        } else {
            fields.push((278, LONG, vec![5]));
            (0..HEIGHT.div_ceil(5)).map(|s| (WIDTH, 5.min(HEIGHT - s * 5))).collect()
        };
        let planes = if l.planar { l.samples as usize } else { 1 };
        let chunks: Vec<Vec<u8>> = (0..planes)
            .flat_map(|_| sizes.iter())
            .enumerate()
            .map(|(i, &(w, h))| bytes(((w * spp * l.bits).div_ceil(8) * h) as usize, 77 + i as u64))
            .collect();
        let coded: Vec<Vec<u8>> =
            chunks.iter().map(|c| ruzstd::encoding::compress_to_vec(&c[..], ruzstd::encoding::CompressionLevel::Fastest)).collect();
        let coded: Vec<&[u8]> = coded.iter().map(|c| &c[..]).collect();
        (tiff_file(big_endian, fields, tiled, &coded).unwrap(), chunks)
    }

    /// The same image, uncompressed.
    fn plain_tiff(zstd: &[u8], l: Layout, tiled: bool, big_endian: bool, chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut dec = Decoder::new(Cursor::new(zstd)).unwrap();
        let mut fields = Vec::new();
        for tag in [256u16, 257, 258, 262, 277, 284, 317, 320, 322, 323, 278, 339] {
            if let Some(v) = dec.find_tag_unsigned_vec::<u32>(Tag::from_u16_exhaustive(tag)).unwrap() {
                fields.push((tag, if matches!(tag, 256 | 257 | 322 | 323 | 278) { LONG } else { SHORT }, v));
            }
        }
        fields.push((259, SHORT, vec![1]));
        let _ = l;
        let chunks: Vec<&[u8]> = chunks.iter().map(|c| &c[..]).collect();
        tiff_file(big_endian, fields, tiled, &chunks).unwrap()
    }

    /// The same sample type and the same bits (of NaNs too).
    fn same(a: &mut DecodingResult, b: &mut DecodingResult) -> bool {
        std::mem::discriminant(a) == std::mem::discriminant(b) && a.as_buffer(0).as_bytes() == b.as_buffer(0).as_bytes()
    }

    /// Every chunk decodes to what the `tiff` crate decodes from the same image stored
    /// uncompressed (and, with the `libzstd` feature, from the Zstandard image with the C
    /// library): the same sample type and values, in strips and tiles of either byte order.
    #[test]
    fn zstd_chunks_decode_as_the_tiff_crate_decodes_them() {
        let (mut decoded, mut refused) = (0, 0);
        for &l in LAYOUTS {
            for (tiled, big_endian) in [(false, false), (false, true), (true, false), (true, true)] {
                let case = format!("{l:?} tiled {tiled} big endian {big_endian}");
                let (file, chunks) = zstd_tiff(l, tiled, big_endian);
                let plain = plain_tiff(&file, l, tiled, big_endian, &chunks);
                let mut dec = Decoder::new(Cursor::new(&file[..])).unwrap_or_else(|e| panic!("{case}: {e}"));
                let mut reference = Decoder::new(Cursor::new(&plain[..])).unwrap();
                for idx in 0..chunks.len() as u32 {
                    // A layout the crate does not decode (a palette of under 8 bits, WhiteIsZero
                    // planes) is refused alike.
                    let mut want = match reference.read_chunk(idx) {
                        Ok(w) => w,
                        Err(e) => {
                            assert!(read_zstd_chunk(&mut dec, idx, usize::MAX).is_err(), "{case}: chunk {idx} {e}");
                            refused += 1;
                            continue;
                        }
                    };
                    let mut got = read_zstd_chunk(&mut dec, idx, usize::MAX).unwrap_or_else(|e| panic!("{case}: chunk {idx}: {e}"));
                    assert!(same(&mut got, &mut want), "{case}: chunk {idx}");
                    // What `read_chunk` gives in this build, through the C library or not.
                    let mut got = read_chunk(&mut dec, idx, usize::MAX).unwrap();
                    assert!(same(&mut got, &mut want), "{case}: chunk {idx} through read_chunk");
                    #[cfg(feature = "libzstd")]
                    {
                        let mut native = dec.read_chunk(idx).unwrap_or_else(|e| panic!("{case}: libzstd chunk {idx}: {e}"));
                        assert!(same(&mut native, &mut want), "{case}: chunk {idx} with libzstd");
                    }
                    decoded += 1;
                }
            }
        }
        // Refused: the 4-bit palette and the WhiteIsZero image of two planes.
        assert_eq!((decoded, refused), (440, 66));
    }

    /// A chunk decompressing past the limit is refused, a missing or truncated one is an error.
    #[test]
    fn oversized_missing_and_truncated_chunks_are_errors() {
        let l = layout(8, 1, 1, 0, 1, false);
        let (file, chunks) = zstd_tiff(l, false, false);
        let mut dec = Decoder::new(Cursor::new(&file[..])).unwrap();
        assert!(read_zstd_chunk(&mut dec, 0, chunks[0].len()).is_ok());
        assert!(matches!(read_zstd_chunk(&mut dec, 0, chunks[0].len() - 1), Err(TiffError::LimitsExceeded)));
        assert!(read_zstd_chunk(&mut dec, 99, usize::MAX).is_err());
        // The last strip cut short.
        let cut = &file[..file.len() - 4];
        let mut dec = Decoder::new(Cursor::new(cut)).unwrap();
        assert!(read_zstd_chunk(&mut dec, chunks.len() as u32 - 1, usize::MAX).is_err());
        // A strip whose stream expands to 32 MiB past its 8 x 2 samples decodes to the samples.
        let stream = ruzstd::encoding::compress_to_vec(&vec![9u8; 16 + (32 << 20)][..], ruzstd::encoding::CompressionLevel::Fastest);
        let fields = vec![
            (256, LONG, vec![8]),
            (257, LONG, vec![2]),
            (258, SHORT, vec![8]),
            (259, SHORT, vec![u32::from(ZSTD)]),
            (262, SHORT, vec![1]),
            (277, SHORT, vec![1]),
            (278, LONG, vec![2]),
        ];
        let bomb = tiff_file(false, fields, false, &[&stream]).unwrap();
        let mut dec = Decoder::new(Cursor::new(&bomb[..])).unwrap();
        assert!(matches!(read_zstd_chunk(&mut dec, 0, 16).unwrap(), DecodingResult::U8(v) if v == [9; 16]));
        assert!(matches!(read_zstd_chunk(&mut dec, 0, 15), Err(TiffError::LimitsExceeded)));
    }

    #[test]
    fn frames_are_read_one_after_another() {
        let (a, b) = (bytes(50_000, 3), bytes(20_000, 9));
        let fastest = |d: &[u8]| ruzstd::encoding::compress_to_vec(d, ruzstd::encoding::CompressionLevel::Fastest);
        let mut skippable = 0x184D_2A50u32.to_le_bytes().to_vec();
        skippable.extend_from_slice(&5u32.to_le_bytes());
        skippable.extend([7; 5]);
        let stream = [fastest(&a), skippable.clone(), fastest(&b)].concat();
        let both = [a.clone(), b].concat();
        assert_eq!(frames(&stream, usize::MAX).unwrap(), both);
        assert_eq!(frames(&stream, 60_000).unwrap(), both[..60_000]);
        assert!(frames(&[fastest(&a), skippable[..10].to_vec()].concat(), usize::MAX).is_err());
    }
}
