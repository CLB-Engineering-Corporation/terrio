# TIFF LZW encoder compatibility

The writer specializes whole independent chunks for TIFF's MSB, eight-bit alphabet,
early-change LZW. Its public writer API and ordered parallel strip/tile collection
are unchanged. A reusable 8192-slot packed-key hash table (32 KiB) and successor
counts (4 KiB) replace the general-purpose weezl encoder's tree. Linear probing
always has space: at most 3838 dictionary entries are live. Every chunk starts
with clear code 256, ends with EOI 257, and resets its dictionary independently.
No unsafe code or new dependency is needed.
Compressed outputs start with at most 4 KiB capacity and grow as needed: ordered
collection must not retain a raw-sized buffer for every sparse strip.

Exact output compatibility includes weezl 0.2.1's sparse-node promotion behavior:
the seventeenth successor consumes a code but is not entered in the lookup table.
Later occurrences therefore miss again. Removing that quirk is a bitstream change.
The final EOI width adjustment and insertion of code 4096 immediately before clear
are also part of the compatibility contract. The latter entry cannot be referenced
and need not be stored. Keep weezl as the test oracle; a dependency upgrade must
revalidate bytes rather than silently changing that contract.

Validation:

```sh
cargo test -p terrio
cargo test -p terrio --lib tiff_lzw
cargo test -p terrio --test lzw_write_reuse
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Under unit tests every specialized encode call asserts exact compressed bytes
against weezl, including existing writer tests. Independent differential tests cover
all prefix lengths 0 through 4095, selected larger strip boundaries, 64 randomized
streams, dictionary clears, sparse-node promotion, constant and periodic inputs,
plus decode round trips. Writer tests cover predicted 8/16/32-bit sample bytes and
float NaNs; the retained 48 complete baseline TIFF/COG fixtures exercise one and
three threads. The TIFF writer emits little-endian files. Big-endian sample-byte
coverage at the encoder layer does not claim a big-endian writer feature.

`lzw_source_profile` separates whole-file reads from in-memory decode in 256-row
windows over a supplied newline-delimited file list. Its complete-source traversal
is a diagnostic, not a measurement of compose's clipped source windows or CPU
shares. Synthetic in-memory writer workloads perform no source I/O or decode.
Performance evidence belongs with workload identities and raw paired measurements;
no cross-platform performance guarantee follows from the encoder design.
