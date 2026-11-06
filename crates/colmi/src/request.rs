//! Requests. Most go to `UART_WRITE` as 16-byte packets; big-data requests go to `DATA_WRITE` as
//! they are.

use crate::cmd;
use crate::packet::{bcd, build, LEN};
use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

/// The ring keeps local wall-clock time; its history is dated in it.
pub fn set_time(now: NaiveDateTime) -> [u8; LEN] {
    build(
        cmd::SET_TIME,
        &[bcd(now.year() as u32 % 100), bcd(now.month()), bcd(now.day()), bcd(now.hour()), bcd(now.minute()), bcd(now.second())],
    )
}

pub fn battery() -> [u8; LEN] {
    build(cmd::BATTERY, &[])
}

/// Client version and a two-letter name, as the vendor app and Gadgetbridge send after connecting.
pub fn phone_name() -> [u8; LEN] {
    build(cmd::PHONE_NAME, &[0x02, 0x0a, b'R', b'M'])
}

/// Quarter-hour steps, calories and distance for the day `days_ago` days back.
pub fn activity(days_ago: u8) -> [u8; LEN] {
    build(cmd::ACTIVITY_HISTORY, &[days_ago, 0x0f, 0x00, 0x5f, 0x01])
}

/// Heart rate for one local day, asked for by the day's local midnight written as if it were UTC.
pub fn heart_rate(day: NaiveDate) -> [u8; LEN] {
    let midnight = day.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or(0) as u32;
    build(cmd::HEART_RATE_HISTORY, &midnight.to_le_bytes())
}

pub fn hrv(days_ago: u8) -> [u8; LEN] {
    build(cmd::HRV_HISTORY, &(days_ago as u32).to_le_bytes())
}

/// Stress for today only.
pub fn stress() -> [u8; LEN] {
    build(cmd::STRESS_HISTORY, &[])
}

/// Hourly SpO2 range for the past days.
pub fn spo2() -> [u8; 7] {
    [cmd::BIG_DATA, crate::bigdata::SPO2, 0x01, 0x00, 0xff, 0x00, 0xff]
}

/// Sleep sessions with the ring's stages for the past days.
pub fn sleep() -> [u8; 7] {
    [cmd::BIG_DATA, crate::bigdata::SLEEP, 0x01, 0x00, 0xff, 0x00, 0xff]
}

/// Half-hourly skin temperature for the past days.
pub fn temperature() -> [u8; 7] {
    [cmd::BIG_DATA, crate::bigdata::TEMPERATURE, 0x01, 0x00, 0x3e, 0x81, 0x02]
}

const PREF_WRITE: u8 = 0x02;

/// Automatic heart rate every `minutes` (5 to 60 in steps of 5); `None` turns it off.
pub fn heart_rate_interval(minutes: Option<u8>) -> [u8; LEN] {
    match minutes {
        Some(m) => build(cmd::HEART_RATE_INTERVAL, &[PREF_WRITE, 0x01, m]),
        None => build(cmd::HEART_RATE_INTERVAL, &[PREF_WRITE, 0x02, 0]),
    }
}

pub fn spo2_auto(on: bool) -> [u8; LEN] {
    build(cmd::SPO2_AUTO, &[PREF_WRITE, on as u8])
}

pub fn hrv_auto(on: bool) -> [u8; LEN] {
    build(cmd::HRV_AUTO, &[PREF_WRITE, on as u8])
}

pub fn stress_auto(on: bool) -> [u8; LEN] {
    build(cmd::STRESS_AUTO, &[PREF_WRITE, on as u8])
}

/// Temperature takes an extra `03` before the operation.
pub fn temperature_auto(on: bool) -> [u8; LEN] {
    build(cmd::TEMPERATURE_AUTO, &[0x03, PREF_WRITE, on as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        let t = NaiveDate::from_ymd_opt(2024, 8, 18).unwrap().and_hms_opt(21, 5, 9).unwrap();
        assert_eq!(&set_time(t)[..7], &[0x01, 0x24, 0x08, 0x18, 0x21, 0x05, 0x09]);
        // tahnok/colmi_r02_client asked for 2024-08-10 with these bytes.
        let day = NaiveDate::from_ymd_opt(2024, 8, 10).unwrap();
        assert_eq!(&heart_rate(day)[..5], &[0x15, 0x80, 0xad, 0xb6, 0x66]);
        assert_eq!(&activity(2)[..6], &[0x43, 0x02, 0x0f, 0x00, 0x5f, 0x01]);
        assert_eq!(&hrv(3)[..5], &[0x39, 3, 0, 0, 0]);
        assert_eq!(&temperature_auto(true)[..4], &[0x3a, 0x03, 0x02, 0x01]);
        assert_eq!(&heart_rate_interval(Some(5))[..4], &[0x16, 0x02, 0x01, 5]);
    }
}
