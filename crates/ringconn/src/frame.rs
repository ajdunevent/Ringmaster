//! Frame layout (section 4) and the command set (section 6).
//!
//! Never send an opcode that is not listed here: a sweep of unknown opcode `0x21` left a ring
//! unusable until it was recharged and re-paired.

/// Commands end in a literal `0x00` with no checksum. An XOR trailer makes the ring ignore them.
pub mod cmd {
    pub const STATUS: [u8; 3] = [0x01, 0x00, 0x00];
    pub const FETCH: [u8; 3] = [0x07, 0x00, 0x00];
    pub const LIVE_HR: [u8; 3] = [0x06, 0x01, 0x00];
    pub const LIVE_SPO2: [u8; 3] = [0x06, 0x02, 0x00];
    pub const LIVE_OFF: [u8; 3] = [0x06, 0x00, 0x00];
    pub const STATUS_QUERY: [u8; 3] = [0xd0, 0x00, 0x00];
    pub const POLL: [u8; 3] = [0x95, 0x00, 0x00];
    pub const HEARTBEAT_ACK: [u8; 3] = [0x91, 0x00, 0x00];
    pub const ACK_OPTICAL: [u8; 3] = [0xc7, 0x00, 0x00];
    pub const ACK_EPOCH: [u8; 3] = [0xcc, 0x00, 0x00];
    pub const ACK_SPORT: [u8; 3] = [0xcd, 0x00, 0x00];

    pub fn auth_reply(r: [u8; 3]) -> [u8; 6] {
        [0x01, 0x01, r[0], r[1], r[2], 0x00]
    }

    pub fn sync_open(cursor: u32, channel: u8) -> [u8; 9] {
        let c = cursor.to_be_bytes();
        [0x02, 0x00, c[0], c[1], c[2], c[3], channel, 0x01, 0x00]
    }
}

/// Opcodes of frames the ring sends.
pub mod op {
    pub const DESCRIPTOR_QUERY: u8 = 0x10;
    pub const HEARTBEAT: u8 = 0x11;
    pub const LIVE: u8 = 0x15;
    pub const OPTICAL_PAGE: u8 = 0x47;
    pub const EPOCH_PAGE: u8 = 0x4c;
    pub const SPORT_PAGE: u8 = 0x4d;
    pub const END_OF_HISTORY: u8 = 0x50;
    pub const STATUS: u8 = 0x81;
    pub const SYNC_OPEN: u8 = 0x82;
    pub const MODE: u8 = 0x86;
    pub const DESCRIPTOR_FETCH: u8 = 0x87;
}

pub fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |acc, b| acc ^ b)
}

/// `None` for frames without a trailer (`0x50`) or too short to judge.
pub fn checksum_ok(frame: &[u8]) -> Option<bool> {
    if frame.len() < 2 || frame[0] == op::END_OF_HISTORY {
        return None;
    }
    let (body, last) = frame.split_at(frame.len() - 1);
    Some(xor(body) == last[0])
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    text.split_whitespace().map(|b| u8::from_str_radix(b, 16).ok()).collect()
}

pub(crate) fn u16_be(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

pub(crate) fn u32_be(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(checksum_ok(&[0x86, 0x00, 0x86]), Some(true));
        assert_eq!(checksum_ok(&[0x82, 0xff, 0x00, 0x7d]), Some(true));
        assert_eq!(checksum_ok(&[0x86, 0x00, 0x87]), Some(false));
        assert_eq!(checksum_ok(&[0x50, 0x00, 0x6a, 0x15]), None);
    }

    #[test]
    fn sync_open_layout() {
        assert_eq!(cmd::sync_open(0x0c2298c3, 0x03), [0x02, 0x00, 0x0c, 0x22, 0x98, 0xc3, 0x03, 0x01, 0x00]);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_hex("4c 0f bc"), Some(vec![0x4c, 0x0f, 0xbc]));
        assert_eq!(hex(&[0x4c, 0x0f, 0xbc]), "4c 0f bc");
        assert_eq!(parse_hex("4c zz"), None);
    }
}
