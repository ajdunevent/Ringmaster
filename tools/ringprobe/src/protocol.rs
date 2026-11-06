//! The probe's view of the protocol: `ringconn` plus labels for the report and the second epoch
//! constant under test.

pub use ringconn::frame::{checksum_ok, cmd, hex};
pub use ringconn::records::{Descriptor, Epoch, EpochLayout, RingState, EPOCH_LEN, OPTICAL_LEN, SPORT_LEN};
pub use ringconn::{COMMAND_CHAR, DATA_SERVICE, NOTIFY_CHAR};
use uuid::{uuid, Uuid};

pub const SECONDARY_SERVICE: Uuid = uuid!("1d14d6ee-fd63-4fa1-bfa4-8f47b42119f0");

/// The two epoch constants in circulation, 4 h apart (PROTOCOL.md section 7.5).
pub const EPOCH_A: i64 = ringconn::time::EPOCH;
pub const EPOCH_B: i64 = 1_577_808_000;

pub const CHANNEL_SLEEP: u8 = 0x00;
pub const CHANNEL_SPORT: u8 = 0x02;
pub const CHANNEL_ALL_DAY: u8 = 0x03;

pub fn auth_response(mac: [u8; 6], challenge: u8) -> [u8; 3] {
    ringconn::auth::response(mac, challenge)
}

pub fn cursor_now(unix_now: i64) -> u32 {
    ringconn::time::cursor(unix_now)
}

pub fn frame_name(op: u8) -> &'static str {
    match op {
        0x10 => "descriptor (d0)",
        0x11 => "heartbeat",
        0x12 => "raw PPG",
        0x15 => "live sample",
        0x47 => "optical page",
        0x4c => "epoch page",
        0x4d => "sport page",
        0x4e => "live sport",
        0x50 => "end of history",
        0x81 => "status reply",
        0x82 => "sync-open reply",
        0x86 => "mode reply",
        0x87 => "descriptor (07)",
        0x8b => "vibrate reply",
        _ => "",
    }
}

pub fn command_name(op: u8) -> &'static str {
    match op {
        0x01 => "status / auth",
        0x02 => "sync open",
        0x06 => "mode",
        0x07 => "fetch",
        0x91 => "heartbeat ack",
        0x95 => "live poll",
        0xc7 | 0xcc | 0xcd | 0xce => "page ack",
        0xd0 => "status query",
        _ => "",
    }
}

pub fn state_name(d: &Descriptor) -> &'static str {
    match d.ring_state() {
        RingState::Settling => "settling",
        RingState::Idle => "idle",
        RingState::Charging => "charging",
        RingState::Sport => "sport",
        RingState::Unknown(_) => "unknown",
    }
}

pub fn case_text(d: &Descriptor) -> String {
    match d.case_battery() {
        None => "not in case".into(),
        Some((pct, plugged)) => format!("case {pct}%{}", if plugged { " plugged in" } else { "" }),
    }
}

/// Records in a history page; a trailing partial record is ignored.
pub fn page_records(page: &[u8], record_len: usize) -> Vec<&[u8]> {
    if page.len() < 4 {
        return Vec::new();
    }
    page[3..page.len() - 1].chunks_exact(record_len).collect()
}

pub fn epochs(page: &[u8]) -> Vec<Epoch> {
    page_records(page, EPOCH_LEN).into_iter().filter_map(Epoch::parse).collect()
}

pub fn counters(page: &[u8], record_len: usize) -> Vec<u32> {
    page_records(page, record_len).into_iter().map(|r| u32::from_be_bytes([r[0], r[1], r[2], r[3]])).collect()
}
