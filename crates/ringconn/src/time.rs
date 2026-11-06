//! Ring time: seconds since an epoch, as in record counters and sync cursors (section 7.5).

/// 2019-12-31 12:00:00 UTC. The 4 h later constant `1577808000` dates Gen 2 FR02.018 records
/// into the future.
pub const EPOCH: i64 = 1_577_793_600;

pub fn to_unix(counter: u32) -> i64 {
    EPOCH + counter as i64
}

pub fn cursor(unix_now: i64) -> u32 {
    (unix_now - EPOCH) as u32
}
