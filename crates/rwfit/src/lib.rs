//! RWfit rings, the family the RWFit app (`com.rw.revivalfit`) serves, sold under many brands.
//! Protocol facts from PulseLoop (CC BY 4.0) and the vendor's published SDK; see
//! `docs/RWFIT.md`. Every multi-byte field is big-endian.

pub mod jieli;
pub mod legacy;
pub mod records;

use uuid::Uuid;

/// Data service, both framings.
pub const SERVICE: Uuid = Uuid::from_u128(0x0000a00a_0000_1000_8000_00805f9b34fb);
/// Commands are written here.
pub const WRITE: Uuid = Uuid::from_u128(0x0000b002_0000_1000_8000_00805f9b34fb);
/// Replies and pushes arrive here.
pub const NOTIFY: Uuid = Uuid::from_u128(0x0000b003_0000_1000_8000_00805f9b34fb);

/// Services found only on rings that use the JieLi framing; the first frame that passes its CRC
/// confirms it.
pub const JIELI_HINTS: [Uuid; 3] = [
    Uuid::from_u128(0x0000ae00_0000_1000_8000_00805f9b34fb),
    Uuid::from_u128(0x00010203_0405_0607_0809_0a0b0c0d1912),
    Uuid::from_u128(0x0000ff00_0000_1000_8000_00805f9b34fb),
];

/// Company IDs in the manufacturer data, with the bytes that follow them.
const MANUFACTURER: [(u16, [u8; 2]); 3] = [(0x05d6, [0x02, 0x00]), (0x05d6, [0x41, 0x54]), (0x06d6, [0x02, 0x00])];

/// Whether an advertisement belongs to an RWfit ring. Names are not used: rebranders change them
/// (the R12 tested on 2026-11-03 advertises as "SR16").
pub fn is_rwfit<'a>(services: impl IntoIterator<Item = &'a Uuid>, manufacturer: impl IntoIterator<Item = (u16, &'a [u8])>) -> bool {
    services.into_iter().any(|u| *u == SERVICE)
        || manufacturer.into_iter().any(|(id, data)| MANUFACTURER.iter().any(|(mid, lead)| *mid == id && data.starts_with(lead)))
}

/// The two wire framings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `0x7E`, XOR checksum, every frame acknowledged both ways.
    Legacy,
    /// `0xAB`, CRC-16/ARC.
    JieLi,
}

impl Framing {
    /// The framing the services suggest; a hint only.
    pub fn from_services<'a>(services: impl IntoIterator<Item = &'a Uuid>) -> Self {
        if services.into_iter().any(|u| JIELI_HINTS.contains(u)) {
            Framing::JieLi
        } else {
            Framing::Legacy
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_r12() {
        // Captured 2026-11-03 from the "SR16".
        let data = [0x02, 0x00, 0x81, 0x00, 0x52, 0x38, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert!(is_rwfit([], [(0x06d6, &data[..])]));
        assert!(is_rwfit([&SERVICE], []));
        assert!(!is_rwfit([], [(0x05a7, &data[..])]));
        assert!(!is_rwfit([], [(0x06d6, &[0x03, 0x00][..])]));
    }

    #[test]
    fn framing_hint() {
        let heart_rate = Uuid::from_u128(0x0000180d_0000_1000_8000_00805f9b34fb);
        assert_eq!(Framing::from_services([&SERVICE, &heart_rate]), Framing::Legacy);
        assert_eq!(Framing::from_services([&SERVICE, &JIELI_HINTS[0]]), Framing::JieLi);
    }
}
