//! 16-byte command packets: command byte, up to 14 bytes of payload, then the low byte of the sum
//! of the first 15 bytes.

pub const LEN: usize = 16;

pub fn build(command: u8, payload: &[u8]) -> [u8; LEN] {
    let mut p = [0u8; LEN];
    p[0] = command;
    let n = payload.len().min(LEN - 2);
    p[1..1 + n].copy_from_slice(&payload[..n]);
    p[LEN - 1] = checksum(&p[..LEN - 1]);
    p
}

pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))
}

/// A full-length packet whose last byte matches.
pub fn valid(p: &[u8]) -> bool {
    p.len() == LEN && checksum(&p[..LEN - 1]) == p[LEN - 1]
}

/// Two-digit decimal numbers are sent as hex digits: 24 becomes `0x24`.
pub fn bcd(v: u32) -> u8 {
    (((v / 10) % 10) << 4 | (v % 10)) as u8
}

pub fn from_bcd(b: u8) -> u32 {
    (b >> 4) as u32 * 10 + (b & 0x0f) as u32
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_request() {
        let p = build(0x03, &[]);
        assert_eq!(p[0], 0x03);
        assert_eq!(p[15], 0x03);
        assert!(valid(&p));
        // Reply captured by tahnok/colmi_r02_client: 64 %.
        let reply = [0x03, 0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x43];
        assert!(valid(&reply));
    }

    #[test]
    fn binary_coded_decimal() {
        assert_eq!((bcd(24), bcd(8), from_bcd(0x59)), (0x24, 0x08, 59));
    }
}
