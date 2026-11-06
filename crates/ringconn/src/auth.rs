//! Per-connection challenge/response (section 5). The only key material is the ring's MAC.

use sm3::{Digest, Sm3};

/// `r = SM3(V || challenge)[29..32]` with `V = mac[3] ^ mac[4] ^ mac[5]`, MAC in display order
/// (`F8:79:99:...` is `[0xf8, 0x79, 0x99, ...]`).
pub fn response(mac: [u8; 6], challenge: u8) -> [u8; 3] {
    let v = mac[3] ^ mac[4] ^ mac[5];
    let digest = Sm3::digest([v, challenge]);
    [digest[29], digest[30], digest[31]]
}

/// The full reply to an `81 00 <challenge> <xor>` frame, or `None` if the frame is not one.
pub fn reply_to(status: &[u8], mac: [u8; 6]) -> Option<[u8; 6]> {
    match status {
        [0x81, 0x00, challenge, ..] => Some(crate::frame::cmd::auth_reply(response(mac, *challenge))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_vectors() {
        let mac = [0xf8, 0x79, 0x99, 0xf7, 0x03, 0xad];
        assert_eq!(response(mac, 0xe5), [0x52, 0x0b, 0xe1]);
        assert_eq!(response(mac, 0xb0), [0x31, 0x82, 0x67]);
    }

    #[test]
    fn accepted_by_gen2_fr02018() {
        let mac = [0xf8, 0x79, 0x99, 0xf8, 0x55, 0xbb];
        assert_eq!(reply_to(&[0x81, 0x00, 0x59, 0xd8], mac), Some([0x01, 0x01, 0x2e, 0xe9, 0xdc, 0x00]));
        assert_eq!(reply_to(&[0x81, 0x01, 0x00], mac), None);
    }
}
