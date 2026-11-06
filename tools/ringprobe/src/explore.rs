//! Exploration run: commands and modes the official app uses whose effect on the Gen 2 is unknown,
//! one after another, every frame logged. Built for one sitting instead of a test per message.
//!
//! Unknown history page types (`40` to `4f` other than `47`, `4c`, `4d`) are acknowledged so the
//! ring sends the next one; the ring then discards those pages. Known history pages are never
//! acknowledged. Firmware update (`09`), system control (`08`) and bind (`03`) are not sent.

use crate::protocol::{self, Descriptor};
use crate::report::Report;
use crate::transport::Link;
use crate::Shared;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

pub struct Plan {
    /// Time worn in work mode 2 before asking for recorded apnea data.
    pub wear_minutes: u64,
}

/// History pages acknowledged per step, so a ring with weeks of data cannot keep a step going.
const MAX_ACKED_PAGES: usize = 300;
const KNOWN_PAGES: [u8; 3] = [0x47, 0x4c, 0x4d];

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn work_mode(mode: u8, byte: u8) -> [u8; 9] {
    let mut f = ringconn::frame::cmd::sync_open(ringconn::time::cursor(now_unix()), byte);
    f[1] = mode;
    f
}

/// The app's request for recorded apnea data, `0a`; its payload is not decoded, so several are
/// tried.
fn offline_requests() -> Vec<Vec<u8>> {
    let c = ringconn::time::cursor(now_unix()).to_be_bytes();
    vec![
        vec![0x0a, 0x00, 0x00],
        vec![0x0a, 0x01, 0x00],
        vec![0x0a, 0x00, c[0], c[1], c[2], c[3], 0x00],
        vec![0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00],
    ]
}

struct Outcome {
    frames: Vec<Vec<u8>>,
    acked: usize,
}

/// Sends `bytes`, then gathers frames for up to `secs` (ending early after 5 s of quiet),
/// acknowledging unknown history pages.
async fn step(link: &mut Link, report: &Report, label: &str, bytes: &[u8], secs: u64) -> Result<Outcome> {
    // A late reply to the previous step would otherwise be read as this step's reply.
    let _ = link.collect(Duration::from_millis(500), Duration::from_secs(2)).await;
    report.note(format!("explore: {label}"));
    if bytes.is_empty() {
        return listen(link, secs).await;
    }
    link.send(bytes).await?;
    listen(link, secs).await
}

/// Frames for up to `secs`, ending after 5 s of quiet unless `secs` is longer than a minute;
/// unknown history pages are acknowledged.
async fn listen(link: &mut Link, secs: u64) -> Result<Outcome> {
    let quiet = if secs > 60 { Duration::from_secs(secs) } else { Duration::from_secs(5) };
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut frames = Vec::new();
    let mut acked = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).min(quiet);
        if left.is_zero() {
            break;
        }
        let Some(f) = link.recv(left).await else { break };
        let op = f.first().copied().unwrap_or(0);
        if (0x40..=0x4f).contains(&op) && !KNOWN_PAGES.contains(&op) && acked < MAX_ACKED_PAGES {
            link.send(&[op | 0x80, 0x00, 0x00]).await?;
            acked += 1;
        }
        frames.push(f);
    }
    Ok(Outcome { frames, acked })
}

fn describe(o: &Outcome) -> String {
    let mut kinds: Vec<(u8, usize, Vec<usize>)> = Vec::new();
    for f in &o.frames {
        let op = f.first().copied().unwrap_or(0);
        match kinds.iter_mut().find(|k| k.0 == op) {
            Some(k) => {
                k.1 += 1;
                if !k.2.contains(&f.len()) && k.2.len() < 4 {
                    k.2.push(f.len());
                }
            }
            None => kinds.push((op, 1, vec![f.len()])),
        }
    }
    if kinds.is_empty() {
        return "no reply".into();
    }
    let mut s = kinds.iter().map(|(op, n, lens)| format!("{op:02x} x{n} {lens:?}")).collect::<Vec<_>>().join(", ");
    // The first frame that is a reply (bit 7 set, not a status frame) shows accepted or refused.
    if let Some(r) = o.frames.iter().find(|f| f.first().is_some_and(|op| op & 0x80 != 0 && *op != 0x87)) {
        s.push_str(&format!("; reply {}", protocol::hex(r)));
    }
    if o.acked > 0 {
        s.push_str(&format!("; {} pages acknowledged", o.acked));
    }
    s
}

async fn state(link: &mut Link) -> String {
    let _ = link.send(&[0x07, 0x00, 0x00]).await;
    match link.wait_for(&[0x87], Duration::from_secs(4)).await.and_then(|f| Descriptor::parse(&f)) {
        Some(d) => format!("{:02x} ({}), battery {} %", d.state, protocol::state_name(&d), d.battery),
        None => "no status frame".into(),
    }
}

pub async fn run(link: &mut Link, report: &Report, shared: &Shared, plan: &Plan) -> Result<()> {
    let mut results: Vec<(String, String)> = Vec::new();
    let mut record = |label: &str, text: String| results.push((label.to_string(), text));
    let started = Instant::now();
    report.note(format!("exploration run; {} min worn in work mode 2", plan.wear_minutes));
    record("state at start", state(link).await);

    // 1. Live measurement modes other than the known 01 and 02.
    for mode in [0x03u8, 0x04, 0x05, 0x07, 0x08, 0x09, 0x0a] {
        shared.live_active.store(true, Ordering::SeqCst);
        let o = step(link, report, &format!("live mode {mode:02x}"), &[0x06, mode, 0x00], 12).await?;
        let _ = link.send(&[0x06, 0x00, 0x00]).await;
        let _ = link.wait_for(&[0x86], Duration::from_secs(3)).await;
        shared.live_active.store(false, Ordering::SeqCst);
        record(&format!("06 {mode:02x} 00"), describe(&o));
    }

    // 2. Factory tests and sensor readouts (the app's replies a0 to a4, a9).
    let mut factory_used = false;
    for (bytes, stop) in [
        (vec![0x29, 0x00, 0x00], None),
        (vec![0x22, 0x01, 0x00], Some(vec![0x22, 0x00, 0x00])),
        (vec![0x22, 0x00, 0x00], None),
        (vec![0x23, 0x01, 0x00], Some(vec![0x23, 0x00, 0x00])),
        (vec![0x23, 0x00, 0x00], None),
    ] {
        let o = step(link, report, &format!("factory {}", protocol::hex(&bytes)), &bytes, 12).await?;
        if o.frames.iter().any(|f| f.len() > 4 && matches!(f.first(), Some(0xa2 | 0xa3))) {
            factory_used = true;
        }
        if let Some(stop) = stop {
            let _ = link.send(&stop).await;
            let _ = link.collect(Duration::from_secs(2), Duration::from_secs(3)).await;
        }
        record(&protocol::hex(&bytes), describe(&o));
    }
    if factory_used {
        let o = step(link, report, "leave factory mode", &[0x20, 0x00, 0x00], 5).await?;
        record("20 00 00", describe(&o));
    }

    // 3. Recorded apnea data, before any apnea mode.
    for req in offline_requests() {
        let o = step(link, report, &format!("offline apnea request {}", protocol::hex(&req)), &req, 15).await?;
        record(&format!("{} (normal state)", protocol::hex(&req)), describe(&o));
    }

    // 4. History channels not yet seen (known: 00, 02, 03).
    for ch in [0x01u8, 0x04, 0x05, 0x06, 0x07, 0x08] {
        let f = work_mode(0x00, ch);
        let o = step(link, report, &format!("sync open channel {ch:02x}"), &f, 10).await?;
        record(&format!("channel {ch:02x}"), describe(&o));
    }

    // 5. Work mode 2 (apnea recording, state 05), with checks inside it.
    let o = step(link, report, "work mode 2", &work_mode(0x02, 0x00), 5).await?;
    record("work mode 2", describe(&o));
    record("state in work mode 2", state(link).await);
    for mode in [0x04u8, 0x05] {
        let o = step(link, report, &format!("live mode {mode:02x} in work mode 2"), &[0x06, mode, 0x00], 8).await?;
        let _ = link.send(&[0x06, 0x00, 0x00]).await;
        record(&format!("06 {mode:02x} 00 (work mode 2)"), describe(&o));
    }
    for req in offline_requests() {
        let o = step(link, report, &format!("offline apnea request {} in work mode 2", protocol::hex(&req)), &req, 10).await?;
        record(&format!("{} (work mode 2, at start)", protocol::hex(&req)), describe(&o));
    }

    // 6. Worn in work mode 2; unprompted frames are logged, the state read every minute.
    report.note(format!("explore: worn in work mode 2 for {} min; keep the ring on", plan.wear_minutes));
    let wear_end = Instant::now() + Duration::from_secs(plan.wear_minutes * 60);
    let mut unprompted: Vec<Vec<u8>> = Vec::new();
    let mut states = Vec::new();
    while Instant::now() < wear_end {
        let minute = Instant::now() + Duration::from_secs(60);
        while let Some(f) =
            link.recv(minute.saturating_duration_since(Instant::now()).min(wear_end.saturating_duration_since(Instant::now()))).await
        {
            let op = f.first().copied().unwrap_or(0);
            if !matches!(op, 0x10 | 0x11 | 0x87) {
                unprompted.push(f);
            }
        }
        states.push(state(link).await);
        if !link.is_open() {
            break;
        }
    }
    record("unprompted frames while worn", describe(&Outcome { frames: unprompted, acked: 0 }));
    record("states while worn", states.join(" | "));

    // 7. Recorded apnea data after wearing.
    for req in offline_requests() {
        let o = step(link, report, &format!("offline apnea request {} after wearing", protocol::hex(&req)), &req, 60).await?;
        record(&format!("{} (work mode 2, after wearing)", protocol::hex(&req)), describe(&o));
    }
    for ch in [0x01u8, 0x04, 0x05] {
        let o = step(link, report, &format!("work mode 2, channel {ch:02x}"), &work_mode(0x02, ch), 15).await?;
        record(&format!("work mode 2 channel {ch:02x}"), describe(&o));
    }

    // 8. Back to normal: a mode-0 sync open on the sleep channel (pages read, not acknowledged),
    // then mode 1, which the app sends at connect, if the state is still 05.
    let o = step(link, report, "work mode 0 (sync open, channel 00)", &work_mode(0x00, 0x00), 15).await?;
    record("work mode 0", describe(&o));
    let after = state(link).await;
    record("state after work mode 0", after.clone());
    if after.starts_with("05") {
        let o = step(link, report, "work mode 1", &work_mode(0x01, 0x03), 5).await?;
        record("work mode 1", describe(&o));
        record("state after work mode 1", state(link).await);
    }

    record("duration", format!("{:.1} min", started.elapsed().as_secs_f64() / 60.0));
    let width = results.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let lines: Vec<String> = results.iter().map(|(k, v)| format!("{k:width$}  {v}")).collect();
    report.finding("Exploration (details in the frame log)", lines.join("\n    "));
    report.section("Exploration summary", lines);
    Ok(())
}

pub struct OsaPlan {
    /// Time worn with apnea monitoring switched on.
    pub minutes: u64,
}

/// Parameter `05 22`: apnea monitoring switch, as the official app (3.10.0) sends it. The app
/// names the values `close` 0, `open` 1, `fastClose` 2, and sends `close` or `fastClose` at
/// connect when no monitoring is running.
const OSA_SWITCH: u8 = 0x22;

/// Apnea monitoring: switched on with the app's parameter, then worn, with every request that
/// might return its data tried before and after; switched off at the end.
pub async fn run_osa(link: &mut Link, report: &Report, plan: &OsaPlan) -> Result<()> {
    let mut results: Vec<(String, String)> = Vec::new();
    let mut record = |label: &str, text: String| results.push((label.to_string(), text));
    let started = Instant::now();
    report.note(format!("apnea monitoring run; {} min worn", plan.minutes));
    record("state at start", state(link).await);

    // 1. Switch on; the frame layout of parameters is inferred, so both lengths are tried.
    let mut on = None;
    for bytes in [vec![0x05, OSA_SWITCH, 0x01, 0x00], vec![0x05, OSA_SWITCH, 0x01]] {
        let o = step(link, report, &format!("apnea switch on {}", protocol::hex(&bytes)), &bytes, 8).await?;
        let accepted = o.frames.iter().any(|f| f.first() == Some(&0x85) && f.get(1) == Some(&0x00));
        record(&protocol::hex(&bytes), describe(&o));
        if accepted {
            on = Some(bytes);
            break;
        }
    }
    record("state after switch on", state(link).await);

    // 2. What the ring sends by itself, and the data requests, right after switching on.
    let o = step(link, report, "listen 60 s", &[], 60).await?;
    record("first minute", describe(&o));
    for (label, bytes) in [("06 04 00", vec![0x06, 0x04, 0x00]), ("06 05 00", vec![0x06, 0x05, 0x00])] {
        let o = step(link, report, label, &bytes, 10).await?;
        let _ = link.send(&[0x06, 0x00, 0x00]).await;
        record(&format!("{label} (switch on)"), describe(&o));
    }
    for req in offline_requests() {
        let o = step(link, report, &format!("request {}", protocol::hex(&req)), &req, 10).await?;
        record(&format!("{} (switch on)", protocol::hex(&req)), describe(&o));
    }
    let o = step(link, report, "work mode 2", &work_mode(0x02, 0x00), 5).await?;
    record("work mode 2", describe(&o));
    record("state after work mode 2", state(link).await);
    let o = step(link, report, "06 04 00 in work mode 2", &[0x06, 0x04, 0x00], 10).await?;
    let _ = link.send(&[0x06, 0x00, 0x00]).await;
    record("06 04 00 (switch on, work mode 2)", describe(&o));

    // 3. Worn; everything the ring sends is kept, the state read every minute.
    report.note(format!("apnea run: worn for {} min; keep the ring on", plan.minutes));
    let mut states = Vec::new();
    let mut frames = Vec::new();
    let mut acked = 0;
    for _ in 0..plan.minutes {
        let o = step(link, report, "listen 1 min", &[], 61).await?;
        acked += o.acked;
        frames.extend(o.frames.into_iter().filter(|f| !matches!(f.first(), Some(0x10 | 0x11 | 0x87))));
        states.push(state(link).await);
        if !link.is_open() {
            break;
        }
    }
    record("frames while worn", describe(&Outcome { frames, acked }));
    record("states while worn", states.join(" | "));

    // 4. Requests after wearing.
    for req in offline_requests() {
        let o = step(link, report, &format!("request {} after wearing", protocol::hex(&req)), &req, 30).await?;
        record(&format!("{} (after wearing)", protocol::hex(&req)), describe(&o));
    }
    for ch in [0x00u8, 0x01, 0x04, 0x05] {
        let o = step(link, report, &format!("work mode 2, channel {ch:02x}"), &work_mode(0x02, ch), 15).await?;
        record(&format!("work mode 2 channel {ch:02x}"), describe(&o));
    }

    // 5. Switch off: fastClose, then close, as the app does at connect.
    let len = on.as_ref().map_or(4, |b| b.len());
    for value in [0x02u8, 0x00] {
        let mut bytes = vec![0x05, OSA_SWITCH, value, 0x00];
        bytes.truncate(len);
        let o = step(link, report, &format!("apnea switch {value}"), &bytes, 15).await?;
        record(&protocol::hex(&bytes), describe(&o));
    }
    let o = step(link, report, "work mode 0", &work_mode(0x00, 0x00), 10).await?;
    record("work mode 0", describe(&o));
    record("state at end", state(link).await);

    record("duration", format!("{:.1} min", started.elapsed().as_secs_f64() / 60.0));
    let width = results.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let lines: Vec<String> = results.iter().map(|(k, v)| format!("{k:width$}  {v}")).collect();
    report.finding("Apnea monitoring (details in the frame log)", lines.join("\n    "));
    report.section("Apnea monitoring summary", lines);
    Ok(())
}

pub struct PpgPlan {
    /// Length of the raw PPG stream.
    pub seconds: u64,
}

/// Raw PPG as the official app (3.10.0) collects it for blood pressure: work mode 2 puts the ring
/// in factory mode (state 05), `22 <mode>` starts the PPG test (the app uses `0a` for Gen 2, which
/// FR02.018 refuses; `08` gives 25 Hz), `23 03` the accelerometer, and `20 01` leaves factory
/// mode.
pub async fn run_ppg(link: &mut Link, report: &Report, plan: &PpgPlan) -> Result<()> {
    let mut results: Vec<(String, String)> = Vec::new();
    let mut record = |label: &str, text: String| results.push((label.to_string(), text));
    let started = Instant::now();
    report.note(format!("raw PPG run; {} s of PPG, restarted whenever it stops", plan.seconds));
    record("state at start", state(link).await);

    // 1. PPG for the planned time. On 2026-10-27 a session ended by itself after about 14 s and
    // the ring left factory mode about 20 s after entering it, so the session is restarted (work
    // mode 2, then `22 08 00`) whenever frames stop for `STALL`.
    const STALL: Duration = Duration::from_millis(2500);
    let end = Instant::now() + Duration::from_secs(plan.seconds);
    let mut sessions: Vec<(f64, f64, usize)> = Vec::new();
    let mut samples = vec!["time,frame,ch1,ch2,ch3,ch4".to_string()];
    let mut others: Vec<Vec<u8>> = Vec::new();
    let mut silent = 0;
    while Instant::now() < end && link.is_open() {
        // On 2026-10-27 BlueZ dropped the link mid-run without reporting it, and writes kept
        // succeeding for five minutes; three restarts without a single frame end the run.
        if silent >= 3 {
            report.note("explore: three restarts without any frame; the link is probably gone");
            break;
        }
        let _ = link.send(&work_mode(0x02, 0x00)).await;
        let _ = link.wait_for(&[0x82], Duration::from_secs(3)).await;
        let _ = link.send(&[0x22, 0x08, 0x00]).await;
        let begun = report.elapsed();
        let (mut last, mut frames) = (begun, 0usize);
        let mut last_ppg = Instant::now();
        loop {
            // Timed from the last PPG frame: live heart-rate frames can keep arriving after it.
            let left = end.saturating_duration_since(Instant::now()).min(STALL.saturating_sub(last_ppg.elapsed()));
            if left.is_zero() {
                break;
            }
            let Some(f) = link.recv(left).await else { break };
            match f.first() {
                Some(0x12) => {
                    last_ppg = Instant::now();
                    last = report.elapsed();
                    frames += 1;
                    for (i, s) in ppg_samples(&f).iter().enumerate() {
                        // Frames arrive about 0.4 s apart with ten samples each.
                        let t = last - (9 - i) as f64 * 0.04;
                        samples.push(format!("{t:.2},{},{},{},{},{}", f[2], s[0], s[1], s[2], s[3]));
                    }
                }
                Some(0x10 | 0x11 | 0x87 | 0x82 | 0xa2) => {}
                _ => others.push(f),
            }
        }
        silent = if frames == 0 { silent + 1 } else { 0 };
        sessions.push((begun, last, frames));
    }
    let _ = link.send(&[0x22, 0x00, 0x00]).await;
    let _ = link.collect(Duration::from_secs(1), Duration::from_secs(3)).await;
    let total: usize = sessions.iter().map(|s| s.2).sum();
    let lengths: Vec<String> = sessions.iter().map(|(b, l, n)| format!("{:.0} s/{n}", (l - b).max(0.0))).collect();
    let gaps: Vec<String> = sessions.windows(2).map(|w| format!("{:.1}", w[1].0 - w[0].1)).collect();
    record("PPG 22 08 00, restarted", format!("{} sessions, {total} frames ({} samples)", sessions.len(), total * 10));
    record("session length/frames", lengths.join(" "));
    record("gaps between sessions (s)", gaps.join(" "));
    record("other frames during PPG", describe(&Outcome { frames: others, acked: 0 }));
    report.section("PPG samples (CSV; time is seconds since start, estimated per sample)", samples);
    record("state after PPG", state(link).await);

    // 2. Accelerometer, briefly.
    let _ = step(link, report, "work mode 2", &work_mode(0x02, 0x00), 3).await?;
    let o = step(link, report, "accelerometer 23 03 00", &[0x23, 0x03, 0x00], 15).await?;
    let streamed: Vec<String> = o.frames.iter().filter(|f| f.first() == Some(&0x13)).take(20).map(|f| protocol::hex(f)).collect();
    if !streamed.is_empty() {
        report.section("Frames from 23 03 00", streamed);
    }
    record("accelerometer 23 03 00", describe(&o));
    let o = step(link, report, "stop 23 00 00", &[0x23, 0x00, 0x00], 5).await?;
    record("23 00 00", describe(&o));

    // 3. Leave factory mode.
    let o = step(link, report, "leave factory mode 20 01 00", &[0x20, 0x01, 0x00], 5).await?;
    record("20 01 00", describe(&o));
    let o = step(link, report, "work mode 0", &work_mode(0x00, 0x00), 10).await?;
    record("work mode 0", describe(&o));
    record("state at end", state(link).await);

    record("duration", format!("{:.1} min", started.elapsed().as_secs_f64() / 60.0));
    let width = results.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let lines: Vec<String> = results.iter().map(|(k, v)| format!("{k:width$}  {v}")).collect();
    report.finding("Raw PPG (details in the frame log)", lines.join("\n    "));
    report.section("Raw PPG summary", lines);
    Ok(())
}

/// Samples of a `12` frame (2026-10-27, Gen 2 in factory mode after `22 08 00`): a 6-byte header
/// (`12 00 <seq> 01 00 <length>`), then 12-byte samples of four 24-bit big-endian channels, then 9
/// bytes of gain settings, a running sample count and a checksum. The fourth channel is signed
/// (20 bits); at 25 Hz the first carries the pulse.
pub(crate) fn ppg_samples(frame: &[u8]) -> Vec<[i32; 4]> {
    if frame.len() < 6 + 9 + 12 {
        return Vec::new();
    }
    let body = &frame[6..frame.len() - 9];
    body.chunks_exact(12)
        .map(|g| {
            let mut s = [0i32; 4];
            for (c, v) in s.iter_mut().enumerate() {
                let raw = i32::from_be_bytes([0, g[c * 3], g[c * 3 + 1], g[c * 3 + 2]]);
                *v = if c == 3 && raw & 0x08_0000 != 0 { raw - 0x10_0000 } else { raw };
            }
            s
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ppg_frame_layout() {
        let f = ringconn::frame::parse_hex(
            "12 00 02 01 00 83 05 17 b9 04 55 83 00 1b 6b 0f ff e4 05 15 38 04 91 f1 00 1d 80 0f ff ec \
             05 14 3f 04 b0 66 00 1e 9d 0f ff ef 05 14 05 04 bf ae 00 1d 2d 00 00 02 05 13 fc 04 c7 76 \
             00 1b 75 0f ff f3 05 13 99 04 cb 5c 00 1c 30 00 00 08 05 13 4f 04 cd b6 00 1e 46 0f ff f2 \
             05 13 4c 04 cf 1c 00 1e 5f 00 00 0b 05 13 9f 04 cf a2 00 1c 70 0f ff f9 05 14 7e 04 d0 41 \
             00 1b 75 0f ff fa 0a 0c 00 0a 00 00 14 00 a3",
        )
        .unwrap();
        let s = super::ppg_samples(&f);
        assert_eq!(s.len(), 10);
        assert_eq!(s[0], [0x0517b9, 0x045583, 0x001b6b, -28]);
        assert_eq!(s[3][3], 2);
    }
}
