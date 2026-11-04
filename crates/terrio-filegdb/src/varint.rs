// SPDX-FileCopyrightText: Copyright (c) 2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 CLB Engineering Corporation
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL ogr/ogrsf_frmts/openfilegdb/: filegdbtable_priv.h, filegdbtable_write.cpp

//! Variable-length integer encodings used by the File Geodatabase format.
//!
//! Ported from GDAL's `ReadVarUInt`, `ReadVarIntAndAddNoCheck` and the
//! corresponding writers in `filegdbtable_priv.h`.

use crate::{Error, Result};

/// A forward-only cursor over a byte slice with bounds-checked reads.
#[derive(Debug, Clone)]
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            Err(Error::Format(format!("unexpected end of buffer: need {n} bytes at offset {}, {} left", self.pos, self.remaining())))
        } else {
            Ok(())
        }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.bytes(2)?.try_into().expect("2 bytes")))
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().expect("2 bytes")))
    }

    pub(crate) fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }

    pub(crate) fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.bytes(8)?.try_into().expect("8 bytes")))
    }

    pub(crate) fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }

    pub(crate) fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().expect("8 bytes")))
    }

    /// Unsigned LEB128-style varint (7 bits per byte, low group first).
    pub(crate) fn varuint(&mut self) -> Result<u64> {
        let mut shift = 0u32;
        let mut val = 0u64;
        loop {
            let b = self.u8()?;
            if shift < 64 {
                val |= u64::from(b & 0x7F) << shift;
            }
            if b & 0x80 == 0 {
                return Ok(val);
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Format("varuint overflow".into()));
            }
        }
    }

    pub(crate) fn varuint32(&mut self) -> Result<u32> {
        let v = self.varuint()?;
        u32::try_from(v).map_err(|_| Error::Format(format!("varuint {v} exceeds 32 bits")))
    }

    /// Signed varint: first byte holds 6 value bits plus a sign bit (0x40).
    pub(crate) fn varint(&mut self) -> Result<i64> {
        let b = self.u8()?;
        let negative = b & 0x40 != 0;
        let mut val = u64::from(b & 0x3F);
        if b & 0x80 != 0 {
            let mut shift = 6u32;
            loop {
                let b = self.u8()?;
                if shift < 64 {
                    val |= u64::from(b & 0x7F) << shift;
                }
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift >= 64 {
                    return Err(Error::Format("varint overflow".into()));
                }
            }
        }
        let v = val as i64;
        Ok(if negative { v.wrapping_neg() } else { v })
    }
}

/// Appends an unsigned varint.
pub(crate) fn write_varuint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Appends a signed varint (sign in bit 6 of the first byte).
pub(crate) fn write_varint(out: &mut Vec<u8>, v: i64) {
    let negative = v < 0;
    let mut mag = v.unsigned_abs();
    let first = (mag & 0x3F) as u8 | if negative { 0x40 } else { 0 };
    mag >>= 6;
    if mag == 0 {
        out.push(first);
        return;
    }
    out.push(first | 0x80);
    write_varuint(out, mag);
}

/// Decodes `n_chars` UTF-16LE code units.
pub(crate) fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = (0..bytes.len() / 2).map(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]])).collect();
    String::from_utf16_lossy(&units)
}

/// Encodes a string as UTF-16LE bytes.
pub(crate) fn to_utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varuint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16383, 16384, u32::MAX as u64, 1 << 50] {
            let mut b = Vec::new();
            write_varuint(&mut b, v);
            assert_eq!(Cursor::new(&b).varuint().unwrap(), v);
        }
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0i64, 1, -1, 63, -63, 64, -64, 100_000, -100_000, i64::from(i32::MAX), -(1 << 40)] {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            assert_eq!(Cursor::new(&b).varint().unwrap(), v, "value {v}");
        }
    }

    #[test]
    fn varint_known_bytes() {
        // 0x41 = sign bit + 1 -> -1
        assert_eq!(Cursor::new(&[0x41]).varint().unwrap(), -1);
        // 0x80 0x01 -> 64
        assert_eq!(Cursor::new(&[0x80, 0x01]).varint().unwrap(), 64);
    }
}
