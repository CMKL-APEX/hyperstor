//! The CRC32C of one 4 KiB block (spec/01-implementation-notes.md I376).
//!
//! The engine and the protocol checksum every block of 4096 bytes. The
//! `crc32c` crate's loop serves every length; this one is cut to 4096: three
//! streams of the `crc32` instruction over 1360, 1360 and 1368 bytes (the
//! instruction takes three cycles and starts one a cycle, so three streams
//! keep it busy), then 8 bytes, and the streams joined by two PCLMULQDQ
//! multiplies by constants that say how many bits follow each stream.
//!
//! The method is Peter Cawley's, from "Faster CRC32-C on x86"
//! (<https://www.corsix.org/content/fast-crc32c-4k>, 2022-08-01; his
//! repository github.com/corsix/fast-crc32 is MIT or zlib licensed). The code
//! here is written again in Rust; its two constants are x^n mod P, which
//! `tests::the_constants_are_x_to_the_n_mod_p` computes and compares.
//!
//! Only where the CPU has SSE4.2 and PCLMULQDQ; elsewhere `append` says so
//! and the caller uses the crate.

/// Folds stream A forward over the 1360 + 1368 + 8 bytes after it:
/// `magic((1360 + 1368 + 8) * 8 - 33)`.
#[cfg(target_arch = "x86_64")]
const K_A: u32 = 0x8A07_4012;
/// Folds stream B forward over the 1368 + 8 bytes after it:
/// `magic((1368 + 8) * 8 - 33)`.
#[cfg(target_arch = "x86_64")]
const K_B: u32 = 0x93E1_06A4;

/// `crc` continued over `block`, as `crc32c::crc32c_append` does; `None` on
/// a CPU without SSE4.2 and PCLMULQDQ.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn append(crc: u32, block: &[u8; 4096]) -> Option<u32> {
    if is_x86_feature_detected!("sse4.2") && is_x86_feature_detected!("pclmulqdq") {
        // SAFETY: the CPU has SSE4.2 and PCLMULQDQ, checked above, and
        // `block` is 4096 readable bytes.
        Some(!unsafe { three_way(!crc, block.as_ptr()) })
    } else {
        None
    }
}

/// Not x86-64: the caller uses the crate.
#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub fn append(_crc: u32, _block: &[u8; 4096]) -> Option<u32> {
    None
}

/// The raw accumulator over 4096 bytes: no bit inversion at either end.
///
/// # Safety
/// The CPU has SSE4.2 and PCLMULQDQ, and `buf` points at 4096 readable
/// bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2,pclmulqdq")]
unsafe fn three_way(acc: u32, buf: *const u8) -> u32 {
    use std::arch::x86_64::*;

    #[inline(always)]
    unsafe fn word(p: *const u8) -> u64 {
        // SAFETY: the caller's `p` is inside the 4096 bytes with 8 to spare.
        unsafe { std::ptr::read_unaligned(p as *const u64) }
    }

    // SAFETY: every read below is 8 bytes at an offset of at most 4088: the
    // loop reads `p`, `p + 1360` and `p + 2720` while `p < buf + 1360`, so
    // at most `buf + 1352 + 2720 + 8 = buf + 4080`; then `buf + 4080` and
    // `buf + 4088`, the last 8 bytes.
    unsafe {
        let (mut a, mut b, mut c) = (acc as u64, 0u64, 0u64);
        let mut p = buf;
        let end = buf.add(1360);
        while p < end {
            a = _mm_crc32_u64(a, word(p));
            b = _mm_crc32_u64(b, word(p.add(1360)));
            c = _mm_crc32_u64(c, word(p.add(1360 * 2)));
            p = p.add(8);
        }
        // Join A and B, leaving room for C and the last 8 bytes.
        let k = _mm_setr_epi32(K_A as i32, 0, K_B as i32, 0);
        let va = _mm_clmulepi64_si128::<0x00>(_mm_cvtsi32_si128(a as i32), k);
        let vb = _mm_clmulepi64_si128::<0x10>(_mm_cvtsi32_si128(b as i32), k);
        let ab = _mm_cvtsi128_si64(_mm_xor_si128(va, vb)) as u64;
        // C's last 8 bytes, then C with the joined A and B and the last 8.
        c = _mm_crc32_u64(c, word(p.add(1360 * 2)));
        _mm_crc32_u64(c, ab ^ word(p.add(1360 * 2 + 8))) as u32
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    fn have_the_instructions() -> bool {
        is_x86_feature_detected!("sse4.2") && is_x86_feature_detected!("pclmulqdq")
    }

    /// CRC32C one bit at a time (polynomial 0x82f63b78, reflected), as
    /// `crc32c_append` continues a CRC.
    fn bitwise(crc: u32, data: &[u8]) -> u32 {
        let mut acc = !crc;
        for &b in data {
            acc ^= b as u32;
            for _ in 0..8 {
                acc = (acc >> 1) ^ (0x82f6_3b78 & (acc & 1).wrapping_neg());
            }
        }
        !acc
    }

    /// x^n mod P, bit-reflected, as the `crc32` instruction makes it
    /// (Cawley's `magic`).
    #[target_feature(enable = "sse4.2")]
    fn magic(n: u32) -> u32 {
        use std::arch::x86_64::*;
        let mut crc = 1u32 << (31 - (n & 31));
        if n & 32 != 0 {
            crc = _mm_crc32_u32(crc, 0);
        }
        for _ in 0..(n >> 6) {
            crc = _mm_crc32_u64(crc as u64, 0) as u32;
        }
        crc
    }

    fn block(seed: u32, extra: usize) -> Vec<u8> {
        (0..4096 + extra)
            .map(|i| {
                (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) as u8 ^ (i >> 7) as u8
            })
            .collect()
    }

    #[test]
    fn the_constants_are_x_to_the_n_mod_p() {
        if !have_the_instructions() {
            return;
        }
        // SAFETY: SSE4.2 checked above.
        let (ka, kb) = unsafe {
            (
                magic((1360 + 1368 + 8) * 8 - 33),
                magic((1368 + 8) * 8 - 33),
            )
        };
        assert_eq!(K_A, ka);
        assert_eq!(K_B, kb);
    }

    #[test]
    fn a_4k_block_agrees_with_a_bitwise_crc_at_every_alignment() {
        if !have_the_instructions() {
            return;
        }
        for seed in [1u32, 77, 12345] {
            let data = block(seed, 16);
            for start in 0..16 {
                let d: &[u8; 4096] = data[start..start + 4096].try_into().unwrap();
                for crc in [0u32, 1, 0xdead_beef, !0] {
                    assert_eq!(
                        append(crc, d),
                        Some(bitwise(crc, d)),
                        "start {start} crc {crc:#x}"
                    );
                }
            }
        }
        for fill in [0x00u8, 0xff, 0x5a] {
            let d = [fill; 4096];
            assert_eq!(append(0, &d), Some(bitwise(0, &d)), "fill {fill:#x}");
        }
    }

    /// One set bit in each part of the block, and at each seam between the
    /// streams: the CRC is linear, so a stream joined at the wrong distance
    /// shows in one of these.
    #[test]
    fn a_bit_set_anywhere_counts_where_it_should() {
        if !have_the_instructions() {
            return;
        }
        for at in [
            0usize, 1, 7, 8, 1351, 1352, 1359, 1360, 1361, 2711, 2712, 2719, 2720, 2721, 4079,
            4080, 4087, 4088, 4089, 4095,
        ] {
            for bit in [0u8, 3, 7] {
                let mut d = [0u8; 4096];
                d[at] = 1 << bit;
                assert_eq!(append(0, &d), Some(bitwise(0, &d)), "byte {at} bit {bit}");
            }
        }
    }

    /// The block's path and the crate's agree when one call continues
    /// another: blocks, then the same bytes in one call (which the crate
    /// serves), and a block after bytes of another length.
    #[test]
    fn appending_blocks_agrees_with_one_call_over_the_bytes() {
        let a = block(3, 0);
        let b = block(9, 0);
        let both: Vec<u8> = a.iter().chain(&b).copied().collect();
        assert_eq!(
            crate::crc32c_append(crate::crc32c(&a), &b),
            crate::crc32c(&both)
        );
        let odd = &a[..1000];
        let mut tail = odd.to_vec();
        tail.extend_from_slice(&b);
        assert_eq!(
            crate::crc32c_append(crate::crc32c(odd), &b),
            crate::crc32c(&tail)
        );
        assert_eq!(crate::crc32c(&a), bitwise(0, &a));
    }
}
