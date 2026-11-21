// SPDX-FileCopyrightText: Copyright (c) 2026 Terrio contributors
// SPDX-License-Identifier: MIT

//! Whole-chunk TIFF LZW: MSB, eight-bit alphabet, early change, twelve-bit codes.
//! Width and clear policy match weezl 0.2.1's TIFF encoder (including its final
//! EOI width adjustment). This is an independent implementation, not a port.

pub(super) struct Encoder {
    // Packed (20-bit prefix/symbol key, 12-bit code). Code zero marks empty.
    table: Vec<u32>,
    successors: Vec<u8>,
}

impl Encoder {
    pub(super) fn new() -> Self {
        Self { table: vec![0; 8192], successors: vec![0; 4096] }
    }

    /// Starts an independent strip/tile. No dictionary state crosses chunks.
    pub(super) fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        self.table.fill(0);
        self.successors.fill(0);
        let mut out = Bits { bytes: Vec::with_capacity(data.len().min(4096)), pending: 0, count: 0 };
        let mut width = 9;
        let mut next = 258u16;
        out.put(256, width);
        if let Some((&first, rest)) = data.split_first() {
            let mut prefix = u16::from(first);
            for &symbol in rest {
                let key = (u32::from(prefix) << 8) | u32::from(symbol);
                let mut slot = (key.wrapping_mul(0x9e3779b1) >> 19) as usize;
                loop {
                    let entry = self.table[slot];
                    if entry == 0 {
                        // Weezl inserts entry 4096, then immediately clears; it
                        // can never be referenced and need not be stored.
                        // Compatibility: weezl 0.2.1 consumes a code but omits the
                        // seventeenth successor when promoting a sparse tree node.
                        let count = &mut self.successors[usize::from(prefix)];
                        let omitted = *count == 16;
                        *count = count.saturating_add(1);
                        if next < 4096 && !omitted {
                            self.table[slot] = (key << 12) | u32::from(next);
                        }
                        next += 1;
                        out.put(prefix, width);
                        if next >= (1 << width) && width < 12 {
                            width += 1;
                        }
                        if next > 4096 {
                            out.put(256, width);
                            self.table.fill(0);
                            self.successors.fill(0);
                            next = 258;
                            width = 9;
                        }
                        prefix = u16::from(symbol);
                        break;
                    }
                    if entry >> 12 == key {
                        prefix = (entry & 4095) as u16;
                        break;
                    }
                    slot = (slot + 1) & 8191;
                }
            }
            out.put(prefix, width);
            // The decoder adds one final entry before it reads EOI.
            if next >= (1 << width) - 1 && width < 12 {
                width += 1;
            }
        }
        out.put(257, width);
        if out.count != 0 {
            out.bytes.push((out.pending << (8 - out.count)) as u8);
        }
        #[cfg(test)]
        assert_eq!(
            out.bytes,
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap(),
            "writer LZW oracle"
        );
        out.bytes
    }
}

struct Bits {
    bytes: Vec<u8>,
    pending: u32,
    count: u8,
}

impl Bits {
    fn put(&mut self, code: u16, width: u8) {
        self.pending = (self.pending << width) | u32::from(code);
        self.count += width;
        while self.count >= 8 {
            self.count -= 8;
            self.bytes.push((self.pending >> self.count) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_strips_do_not_retain_raw_sized_output_buffers() {
        let raw = vec![0; 217724];
        let out = Encoder::new().encode(&raw);
        assert!(out.capacity() < raw.len() / 8);
    }

    #[test]
    fn randomized_streams_and_sparse_node_promotion_match() {
        let mut enc = Encoder::new();
        for seed in 1..=64u32 {
            let mut rng = seed;
            let mut data = Vec::new();
            // Repeated prefix zero with many successors explicitly exercises
            // weezl's omitted seventeenth edge, then later encounters it again.
            for _ in 0..3 {
                for b in 1..=255u8 {
                    data.extend([0, b]);
                }
            }
            for _ in 0..(seed as usize * 1733) {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                data.push(rng as u8);
            }
            let actual = enc.encode(&data);
            let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(&data).unwrap();
            assert!(actual == expected, "seed {seed}");
            assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), data);
        }
    }

    #[test]
    fn matches_weezl_and_decodes_across_widths_resets_and_chunk_boundaries() {
        let mut encoder = Encoder::new();
        let mut rng = 0x12345678u32;
        let random: Vec<u8> = (0..1_000_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                rng as u8
            })
            .collect();
        let patterns = [random, vec![0; 1_000_000], vec![255; 1_000_000], (0..1_000_000).map(|i| (i % 251) as u8).collect()];
        for data in &patterns {
            // All short lengths catch EOI at width changes; larger boundaries
            // cover typical strip sizes and repeated dictionary saturation.
            for n in (0..4096).chain([8191, 8192, 8193, 65535, 65536, 65537, 217723, 217724, 217725, 1_000_000]) {
                let input = &data[..n];
                let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(input).unwrap();
                let actual = encoder.encode(input);
                assert!(actual == expected, "length {n}, first difference {:?}", actual.iter().zip(&expected).position(|(a, b)| a != b));
                assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), input);
            }
        }
    }
}
