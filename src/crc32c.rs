//! CRC-32C (Castagnoli) as ext4 and jbd2 use it: the running value, without the final
//! inversion (`crc32c(!0, data)` is what Linux's `ext4_chksum(sbi, ~0, …)` stores).

const fn table() -> [[u32; 256]; 4] {
    let mut t = [[0u32; 256]; 4];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 4 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static TABLE: [[u32; 256]; 4] = table();

/// Continues `crc` over `data`.
pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        crc ^= u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        crc = TABLE[3][(crc & 0xFF) as usize]
            ^ TABLE[2][((crc >> 8) & 0xFF) as usize]
            ^ TABLE[1][((crc >> 16) & 0xFF) as usize]
            ^ TABLE[0][(crc >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc >> 8) ^ TABLE[0][((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    #[test]
    fn check_value() {
        // The standard check: CRC-32C("123456789") = 0xE3069283 (with both inversions).
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        let long: [u8; 37] = core::array::from_fn(|i| (i * 7) as u8);
        let mut bitwise = !0u32;
        for &b in &long {
            bitwise ^= b as u32;
            for _ in 0..8 {
                bitwise = if bitwise & 1 != 0 { (bitwise >> 1) ^ 0x82F6_3B78 } else { bitwise >> 1 };
            }
        }
        assert_eq!(crc32c(!0, &long), bitwise);
    }
}
