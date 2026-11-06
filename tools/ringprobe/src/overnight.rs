//! Overnight test: back-to-back live SpO2 sessions for a set number of hours, with battery
//! readings every 10 minutes, then a short look at the PPG modes (`06 04 00`, then `06 05 00`).
//!
//! No sync channel is opened, so no history page is requested, acknowledged or removed; the ring
//! keeps everything for Ringmaster's next sync.

use crate::protocol::{self, cmd, Descriptor};
use crate::report::Report;
use crate::transport::{self, Link};
use crate::Shared;
use anyhow::Result;
use bluer::{Address, Device};
use chrono::{Local, TimeZone};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub struct Plan {
    pub hours: f64,
    pub min_battery: u8,
    pub ppg_seconds: u64,
    /// Seconds to wait after each session, taken in turn for each `BLOCK` of the night.
    pub pauses: Vec<u64>,
    /// Raw PPG all night instead of live SpO2 sessions; samples go to `<report>.ppg.csv`.
    pub raw_ppg: bool,
    /// Seconds to wait after leaving factory mode, before the next PPG session.
    pub ppg_rest: u64,
}

/// How long the ring may go on taking connections without answering, with no answered one in
/// between, before the run stops. A locked ring (2026-10-27) did this for 7.6 h until it was put
/// in its case; at -93 dBm (2026-10-28) a ring that was not locked did it three times in 3.5 min.
const NO_ANSWER_LIMIT: Duration = Duration::from_secs(20 * 60);
const STOP_NO_ANSWER: &str = "the ring stopped answering commands; seat it in its case for a moment and take it out to restart it";

/// PPG frames stopping this long means the session has ended and is restarted.
const PPG_STALL: Duration = Duration::from_millis(2500);

/// Each pause length runs for this long before the next one takes over, so one night compares
/// several and the ring's epochs can be matched to them afterwards.
const BLOCK: Duration = Duration::from_secs(30 * 60);

/// `06 04` is what the official app (3.10.0) sends to stream PPG (frames `12`) for apnea
/// monitoring; `06 05` is OpenCircuit's 100 Hz raw mode. Both are tried, in that order, after
/// `PPG_WORK_MODE`; a Gen 2 refused both without it.
const RAW_PPG: [[u8; 3]; 2] = [[0x06, 0x04, 0x00], [0x06, 0x05, 0x00]];
/// Work mode, the same command as a sync open: `02 <mode> <cursor:4> <byte> 01 00`. The app sends
/// mode 2 when it starts collecting PPG and mode 0 (a sync open) otherwise, so Ringmaster's next
/// sync puts the ring back.
const PPG_WORK_MODE: u8 = 0x02;
/// Sample `12` frames kept in the report for working out their layout.
const PPG_SAMPLES: usize = 40;
const BATTERY_EVERY: Duration = Duration::from_secs(10 * 60);
/// A session normally ends by itself about 30 s after the mode command.
const SESSION_LIMIT: Duration = Duration::from_secs(45);
const SESSION_QUIET: Duration = Duration::from_secs(8);

struct Session {
    clock: i64,
    started: f64,
    mode_reply: bool,
    /// The ring answered with state `01` (settling) and sent no samples, as when it is not worn.
    declined: bool,
    samples: Vec<(f64, Vec<u8>)>,
    ended: &'static str,
    finished: f64,
    pause: u64,
}

fn clock(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single().map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_else(|| "?".into())
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// SpO2 frame (PROTOCOL.md section 8): `15 01 00 00 <a:4 LE> 00 00 <b:4 LE> <spo2> 00`. The ratio
/// a/b tracks SpO2 (r = 0.85 on 2026-10-22), as the red/infrared ratio of a pulse oximeter does.
fn fields(f: &[u8]) -> Option<(u32, u32, u8)> {
    (f.len() >= 16 && f[0] == 0x15 && f[1] == 0x01)
        .then(|| (u32::from_le_bytes([f[4], f[5], f[6], f[7]]), u32::from_le_bytes([f[10], f[11], f[12], f[13]]), f[14]))
}

fn valid_spo2(v: u8) -> bool {
    (70..=100).contains(&v)
}

/// Another program talking to the ring at the same time would interleave commands with ours.
pub fn ringmaster_running() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "ringmaster"))
}

async fn read_battery(link: &mut Link) -> Option<Descriptor> {
    link.send(&cmd::FETCH).await.ok()?;
    link.wait_for(&[0x87], Duration::from_secs(4)).await.and_then(|f| Descriptor::parse(&f))
}

/// Drops whatever link BlueZ holds and builds a fresh one with notifications and authentication.
async fn reconnect(device: &Device, address: Address, report: &Arc<Report>, shared: &Shared) -> Result<Link> {
    if device.is_connected().await.unwrap_or(false) {
        let _ = tokio::time::timeout(Duration::from_secs(5), device.disconnect()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    transport::connect(device, report).await?;
    transport::wait_ready(device, report, Duration::from_secs(30)).await;
    let mut link = Link::open(device, report.clone()).await?;
    *shared.command.lock().unwrap() = Some(link.command_char());
    crate::authenticate(&mut link, report, shared, address).await?;
    crate::wake_telemetry(&mut link, shared).await?;
    Ok(link)
}

async fn spo2_session(link: &mut Link, report: &Report, shared: &Shared) -> Result<Session> {
    let mut s = Session {
        clock: now_unix(),
        started: report.elapsed(),
        mode_reply: false,
        declined: false,
        samples: Vec::new(),
        ended: "45 s limit",
        finished: 0.0,
        pause: 0,
    };
    link.send(&cmd::STATUS_QUERY).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    link.send(&cmd::LIVE_SPO2).await?;
    shared.live_active.store(true, Ordering::SeqCst);
    let start = Instant::now();
    loop {
        let left = SESSION_LIMIT.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        let wait = if s.samples.is_empty() { left } else { left.min(SESSION_QUIET) };
        let Some(f) = link.recv(wait).await else {
            if !link.is_open() {
                s.ended = "link closed";
                break;
            }
            if !s.samples.is_empty() {
                s.ended = "went quiet";
                break;
            }
            continue;
        };
        match f.first() {
            Some(0x86) => s.mode_reply = true,
            Some(0x15) => s.samples.push((report.elapsed(), f)),
            Some(0x10 | 0x87) if !s.samples.is_empty() && Descriptor::parse(&f).is_some_and(|d| matches!(d.state, 0x02 | 0x03)) => {
                s.ended = "ring ended it";
                break;
            }
            Some(0x10 | 0x87) if s.samples.is_empty() && Descriptor::parse(&f).is_some_and(|d| d.state == 0x01) => {
                s.declined = true;
                s.ended = "ring declined";
                break;
            }
            _ => {}
        }
    }
    s.finished = report.elapsed();
    link.send(&cmd::LIVE_OFF).await?;
    link.wait_for(&[0x86], Duration::from_millis(1500)).await;
    shared.live_active.store(false, Ordering::SeqCst);
    Ok(s)
}

async fn raw_ppg(link: &mut Link, report: &Report, shared: &Shared, seconds: u64, mode: [u8; 3]) -> Result<String> {
    report.note(format!("PPG mode {} for up to {seconds} s", protocol::hex(&mode)));
    link.send(&cmd::STATUS_QUERY).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let started = report.elapsed();
    link.send(&mode).await?;
    // The ring answers a mode it will not run with `86 ff`, as Gen 2 did to `06 04 00` on
    // 2026-10-27; there is nothing to collect then.
    let reply = link.wait_for(&[0x86], Duration::from_secs(3)).await;
    if let Some(r) = reply.as_ref().filter(|r| r.get(1).is_some_and(|b| *b != 0)) {
        let _ = link.send(&cmd::LIVE_OFF).await;
        return Ok(format!("refused by the ring ({})", protocol::hex(r)));
    }
    shared.live_active.store(true, Ordering::SeqCst);
    let frames = link.collect(Duration::from_secs(10), Duration::from_secs(seconds)).await;
    let last = report.elapsed();
    link.send(&cmd::LIVE_OFF).await?;
    let off = link.wait_for(&[0x86], Duration::from_secs(3)).await;
    shared.live_active.store(false, Ordering::SeqCst);
    let after = link.collect(Duration::from_secs(5), Duration::from_secs(10)).await;

    let mut kinds: Vec<(u8, usize, Vec<usize>)> = Vec::new();
    for f in &frames {
        let op = f.first().copied().unwrap_or(0);
        match kinds.iter_mut().find(|k| k.0 == op) {
            Some(k) => {
                k.1 += 1;
                if !k.2.contains(&f.len()) {
                    k.2.push(f.len());
                }
            }
            None => kinds.push((op, 1, vec![f.len()])),
        }
    }
    let ppg = frames.iter().filter(|f| f.first() == Some(&0x12)).count();
    let samples: Vec<String> = frames.iter().filter(|f| f.first() == Some(&0x12)).take(PPG_SAMPLES).map(|f| protocol::hex(f)).collect();
    if !samples.is_empty() {
        report.section(format!("PPG frames from {}", protocol::hex(&mode)), samples);
    }
    let span = last - started;
    let state = after.iter().rev().find_map(|f| Descriptor::parse(f)).map(|d| format!("{:02x} ({})", d.state, protocol::state_name(&d)));
    Ok(format!(
        "{} frames in {span:.0} s ({}); {ppg} raw PPG frames, {:.1} a second; {}; leave-mode reply: {}; ring state afterwards: {}",
        frames.len(),
        kinds.iter().map(|(op, n, lens)| format!("{op:02x} x{n}, length {lens:?}")).collect::<Vec<_>>().join("; "),
        ppg as f64 / span.max(1.0),
        if (span as u64) + 2 < seconds { "stopped by itself before the limit" } else { "still running at the limit" },
        off.map(|f| protocol::hex(&f)).unwrap_or_else(|| "none".into()),
        state.unwrap_or_else(|| "no status frame".into()),
    ))
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

fn pearson(pairs: &[(f64, f64)]) -> Option<f64> {
    let n = pairs.len() as f64;
    if n < 10.0 {
        return None;
    }
    let (mx, my) = (pairs.iter().map(|p| p.0).sum::<f64>() / n, pairs.iter().map(|p| p.1).sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in pairs {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    (sxx > 0.0 && syy > 0.0).then(|| sxy / (sxx * syy).sqrt())
}

pub async fn run(link: &mut Link, device: &Device, address: Address, report: &Arc<Report>, shared: &Shared, plan: &Plan) -> Result<()> {
    report.note(format!(
        "overnight test for {} h: back-to-back {}; no sync channel is opened, so the ring keeps its history",
        plan.hours,
        if plan.raw_ppg { "raw PPG sessions" } else { "live SpO2 sessions" }
    ));
    let begun = Instant::now();
    let end = begun + Duration::from_secs_f64(plan.hours * 3600.0);
    let mut blocks: Vec<(i64, u64)> = Vec::new();
    let mut sessions: Vec<Session> = Vec::new();
    let mut battery: Vec<(i64, u8, u16, u8)> = Vec::new();
    let (mut reconnects, mut failed_reconnects) = (0, 0);
    let mut stop = "the planned time ran out";
    let mut next_battery = Instant::now();
    let mut silent = 0;
    // When the first unanswered connection since the last answered one happened.
    let mut no_answer: Option<Instant> = None;
    let mut ppg_sessions: Vec<PpgSession> = Vec::new();
    let mut csv = None;
    if plan.raw_ppg {
        let path = report.path().with_extension("ppg.csv");
        let mut f = std::fs::File::options().write(true).create_new(true).open(&path)?;
        writeln!(f, "unix_ms,frame,ch1,ch2,ch3,ch4")?;
        report.note(format!("raw PPG all night; samples in {}; PPG frames are counted, not logged here", path.display()));
        report.mute(0x12);
        csv = Some(f);
    }
    while Instant::now() < end {
        let connected = device.is_connected().await.unwrap_or(false);
        if !connected || !link.is_open() || silent >= 2 {
            report.note(if connected { "no replies over the link; rebuilding it" } else { "link lost; reconnecting" });
            silent = 0;
            match reconnect(device, address, report, shared).await {
                Ok(l) => {
                    *link = l;
                    reconnects += 1;
                    no_answer = None;
                }
                Err(e) => {
                    failed_reconnects += 1;
                    if format!("{e:#}").contains(crate::NO_ANSWER) {
                        let since = *no_answer.get_or_insert_with(Instant::now);
                        if since.elapsed() >= NO_ANSWER_LIMIT {
                            stop = STOP_NO_ANSWER;
                            break;
                        }
                    }
                    report.note(format!("reconnect failed: {e:#}; trying again in 30 s"));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            }
        }
        if Instant::now() >= next_battery {
            next_battery = Instant::now() + BATTERY_EVERY;
            match read_battery(link).await {
                Some(d) => {
                    battery.push((now_unix(), d.battery, d.millivolts, d.state));
                    report.note(format!("battery {} % ({} mV, state {:02x})", d.battery, d.millivolts, d.state));
                    if d.battery < plan.min_battery {
                        stop = "the battery fell below the limit";
                        break;
                    }
                }
                None => report.note("battery reading failed"),
            }
        }
        let block = (begun.elapsed().as_secs() / BLOCK.as_secs()) as usize;
        let pause = plan.pauses.get(block % plan.pauses.len().max(1)).copied().unwrap_or(0);
        if blocks.len() <= block && !plan.raw_ppg {
            report.note(format!("block {}: {pause} s pause after each session", block + 1));
            blocks.push((now_unix(), pause));
        }
        if plan.raw_ppg {
            match ppg_session(link, report, csv.as_mut(), plan.ppg_rest).await {
                Ok(s) => {
                    silent = if s.frames > 0 { 0 } else { silent + 1 };
                    ppg_sessions.push(s);
                }
                Err(e) => {
                    report.note(format!("PPG session failed: {e:#}"));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
            continue;
        }
        match spo2_session(link, report, shared).await {
            Ok(mut s) => {
                // Without even a mode reply, the link is not delivering notifications.
                silent = if s.mode_reply { 0 } else { silent + 1 };
                // A ring that is not worn declines at once; the wait keeps the log from filling.
                let wait = if s.declined { pause.max(30) } else { pause };
                s.pause = wait;
                sessions.push(s);
                // Collecting rather than sleeping keeps status frames from the pause out of the
                // next session.
                let left = end.saturating_duration_since(Instant::now());
                let wait = Duration::from_secs(wait).min(left);
                if !wait.is_zero() {
                    link.collect(wait, wait).await;
                }
            }
            Err(e) => {
                report.note(format!("session failed: {e:#}"));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
    report.note(format!("overnight sessions stopped: {stop}"));
    if plan.raw_ppg && stop == STOP_NO_ANSWER {
        summarise_ppg(report, &ppg_sessions, &battery, reconnects, failed_reconnects, stop);
        return Ok(());
    }
    if plan.raw_ppg {
        for bytes in [[0x22u8, 0x00, 0x00], [0x20, 0x01, 0x00]] {
            let _ = link.send(&bytes).await;
            let _ = link.collect(Duration::from_secs(1), Duration::from_secs(3)).await;
        }
        let _ = link.send(&ringconn::frame::cmd::sync_open(ringconn::time::cursor(now_unix()), 0x00)).await;
        let _ = link.collect(Duration::from_secs(2), Duration::from_secs(5)).await;
        summarise_ppg(report, &ppg_sessions, &battery, reconnects, failed_reconnects, stop);
        return Ok(());
    }

    let enough_battery = battery.last().is_none_or(|b| b.1 >= plan.min_battery);
    let ppg = if plan.ppg_seconds == 0 {
        "skipped (--ppg-seconds 0)".to_string()
    } else if !enough_battery {
        "skipped: battery below the limit".to_string()
    } else if !device.is_connected().await.unwrap_or(false) || !link.is_open() {
        "skipped: not connected".to_string()
    } else {
        let mut results = Vec::new();
        let mut open = ringconn::frame::cmd::sync_open(ringconn::time::cursor(now_unix()), 0x00);
        open[1] = PPG_WORK_MODE;
        report.note(format!("work mode {PPG_WORK_MODE}: {}", protocol::hex(&open)));
        let entered = match link.send(&open).await {
            Ok(()) => match link.wait_for(&[0x82], Duration::from_secs(5)).await {
                Some(r) => format!("work mode {PPG_WORK_MODE} reply {}", protocol::hex(&r)),
                None => format!("no reply to work mode {PPG_WORK_MODE}"),
            },
            Err(e) => format!("work mode {PPG_WORK_MODE} not sent: {e:#}"),
        };
        results.push(entered);
        for mode in RAW_PPG {
            let r = raw_ppg(link, report, shared, plan.ppg_seconds, mode).await.unwrap_or_else(|e| format!("failed: {e:#}"));
            results.push(format!("{}: {r}", protocol::hex(&mode)));
        }
        results.join(" | ")
    };
    if let Some(d) = read_battery(link).await {
        battery.push((now_unix(), d.battery, d.millivolts, d.state));
    }

    let mut schedule = vec!["from      pause after each session".to_string()];
    for (start, pause) in &blocks {
        schedule.push(format!("{}  {pause} s", clock(*start)));
    }
    report.section("Overnight pause schedule", schedule);
    summarise(report, &sessions, &battery, reconnects, failed_reconnects, stop, &ppg);
    Ok(())
}

fn summarise(
    report: &Report,
    sessions: &[Session],
    battery: &[(i64, u8, u16, u8)],
    reconnects: usize,
    failed_reconnects: usize,
    stop: &str,
    ppg: &str,
) {
    report.finding(
        "Overnight: history on the ring",
        "No sync channel was opened and no page was acknowledged, so nothing was removed from the ring. Sync in \
         Ringmaster afterwards. If the epochs for these hours are missing, or lack their usual SpO2 every 5 minutes, \
         live sessions displace the ring's own recording.",
    );

    let mut lines = vec!["#     start     mode  frames  valid  first valid  length  ended          gap before  pause".to_string()];
    let mut csv = vec!["clock,session,seconds,spo2,a,b,frame".to_string()];
    let (mut lengths, mut gaps, mut values, mut ratios) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut worn, mut declined, mut silent) = (0usize, 0usize, 0usize);
    let mut worn_seconds = 0.0;
    for (i, s) in sessions.iter().enumerate() {
        let valid: Vec<&(f64, Vec<u8>)> = s.samples.iter().filter(|(_, f)| fields(f).is_some_and(|x| valid_spo2(x.2))).collect();
        let first = valid.first().map(|(t, _)| t - s.started);
        let length = s.finished - s.started;
        // Time between sessions beyond the planned pause.
        let gap = i.checked_sub(1).map(|p| s.started - sessions[p].finished - sessions[p].pause as f64);
        if !valid.is_empty() {
            worn += 1;
            worn_seconds += length + gap.filter(|g| *g < 10.0).unwrap_or(0.0);
            lengths.push(length);
            if let Some(g) = gap.filter(|g| *g < 10.0) {
                gaps.push(g);
            }
        } else if s.declined {
            declined += 1;
        } else if !s.mode_reply {
            silent += 1;
        }
        lines.push(format!(
            "{:<5} {}  {:<4}  {:>6}  {:>5}  {:>11}  {:>5.0}s  {:<13}  {:<10}  {} s",
            i + 1,
            clock(s.clock),
            if s.mode_reply { "yes" } else { "no" },
            s.samples.len(),
            valid.len(),
            first.map(|f| format!("{f:.0} s")).unwrap_or_else(|| "-".into()),
            length,
            s.ended,
            gap.map(|g| format!("{g:.1} s")).unwrap_or_else(|| "-".into()),
            s.pause,
        ));
        for (t, f) in &s.samples {
            if let Some((a, b, spo2)) = fields(f) {
                let at = s.clock + (t - s.started) as i64;
                csv.push(format!("{},{},{:.1},{spo2},{a},{b},{}", clock(at), i + 1, t - s.started, protocol::hex(f).replace(' ', "")));
                if valid_spo2(spo2) {
                    values.push((at, spo2));
                    if a > 0 && b > 0 {
                        ratios.push(((a as f64 / b as f64).ln(), spo2 as f64));
                    }
                }
            }
        }
    }
    report.section("Overnight live SpO2 sessions", lines);
    report.section("Overnight live SpO2 frames (CSV)", csv);

    let worn_hours = worn_seconds / 3600.0;
    report.finding(
        "Overnight: live SpO2 sessions",
        format!(
            "{} sessions ({stop}): {worn} gave readings, {declined} declined by the ring (not worn), {silent} without any reply; \
             while worn: {:.1} h, median session {}, median restart gap {}, {} valid readings, one every {} s; \
             reconnects {reconnects}, failed reconnect attempts {failed_reconnects}",
            sessions.len(),
            worn_hours,
            median(lengths).map(|m| format!("{m:.0} s")).unwrap_or_else(|| "-".into()),
            median(gaps).map(|m| format!("{m:.1} s")).unwrap_or_else(|| "-".into()),
            values.len(),
            if values.is_empty() { "-".into() } else { format!("{:.1}", worn_seconds / values.len() as f64) },
        ),
    );

    // A rough preview of 3-point dips against the median of the previous 2 minutes; not an ODI.
    let mut dips = 0;
    let mut in_dip = false;
    for (i, &(t, v)) in values.iter().enumerate() {
        let window: Vec<f64> = values[..i].iter().rev().take_while(|(u, _)| t - u <= 120).map(|(_, x)| *x as f64).collect();
        if window.len() < 5 {
            continue;
        }
        let Some(base) = median(window) else { continue };
        let low = (v as f64) <= base - 3.0;
        if low && !in_dip {
            dips += 1;
        }
        in_dip = low;
    }
    let spo2: Vec<f64> = values.iter().map(|v| v.1 as f64).collect();
    report.finding(
        "Overnight: SpO2 values",
        format!(
            "lowest {}, median {}, highest {}; {dips} drops of 3 points or more below the previous 2 minutes ({:.1} an hour worn, a rough preview only)",
            spo2.iter().cloned().fold(f64::NAN, f64::min),
            median(spo2.clone()).unwrap_or(f64::NAN),
            spo2.iter().cloned().fold(f64::NAN, f64::max),
            if worn_hours > 0.0 { dips as f64 / worn_hours } else { 0.0 },
        ),
    );
    report.finding(
        "Overnight: ln(a/b) from the SpO2 frame against SpO2",
        pearson(&ratios).map(|r| format!("Pearson r {r:+.2} over {} readings", ratios.len())).unwrap_or_else(|| "too few readings".into()),
    );

    let mut blines = vec!["clock     battery  mV     state".to_string()];
    for (t, pct, mv, state) in battery {
        blines.push(format!("{}  {:>5} %  {:>5}  {state:02x}", clock(*t), pct, mv));
    }
    report.section("Overnight battery", blines);
    let drain = match (battery.first(), battery.last()) {
        (Some(a), Some(b)) if b.0 > a.0 => {
            let h = (b.0 - a.0) as f64 / 3600.0;
            format!("{} % to {} % over {h:.1} h, {:.1} points an hour", a.1, b.1, (a.1 as f64 - b.1 as f64) / h)
        }
        _ => "not enough readings".into(),
    };
    report.finding("Overnight: battery", drain);
    report.finding("PPG (work mode 2, then 06 04 00 and 06 05 00)", ppg);
}

struct PpgSession {
    started: f64,
    first: Option<f64>,
    last: f64,
    frames: usize,
    /// The ring acknowledged `20 01 00` (reply `a0`) after the session.
    left: bool,
}

/// One factory-mode PPG session: work mode 2, then `22 08 00`, until frames stop, then `20 01 00`
/// to leave factory mode and `rest` seconds of waiting. Samples are written to `csv` with times
/// estimated from the frame's arrival, 40 ms apart.
async fn ppg_session(link: &mut Link, report: &Report, mut csv: Option<&mut std::fs::File>, rest: u64) -> Result<PpgSession> {
    let mut open = ringconn::frame::cmd::sync_open(ringconn::time::cursor(now_unix()), 0x00);
    open[1] = PPG_WORK_MODE;
    link.send(&open).await?;
    let _ = link.wait_for(&[0x82], Duration::from_secs(3)).await;
    link.send(&[0x22, 0x08, 0x00]).await?;
    let mut s = PpgSession { started: report.elapsed(), first: None, last: report.elapsed(), frames: 0, left: false };
    // Live heart-rate frames keep coming after the PPG stops, so the stall is timed from the
    // last PPG frame.
    let mut last_ppg = Instant::now();
    loop {
        let left = PPG_STALL.saturating_sub(last_ppg.elapsed());
        if left.is_zero() {
            break;
        }
        let Some(f) = link.recv(left).await else { break };
        if f.first() != Some(&0x12) {
            continue;
        }
        last_ppg = Instant::now();
        s.last = report.elapsed();
        s.first.get_or_insert(s.last);
        s.frames += 1;
        if let Some(out) = csv.as_mut() {
            let now = chrono::Utc::now().timestamp_millis();
            for (i, v) in crate::explore::ppg_samples(&f).iter().enumerate() {
                let t = now - (9 - i as i64) * 40;
                writeln!(out, "{t},{},{},{},{},{}", f[2], v[0], v[1], v[2], v[3])?;
            }
        }
    }
    if let Some(out) = csv {
        out.flush()?;
    }
    // A session that ended because the link dropped is still counted; the caller reconnects.
    if link.send(&[0x20, 0x01, 0x00]).await.is_err() {
        report.note("leaving factory mode failed; the link has probably dropped");
        return Ok(s);
    }
    s.left = link.wait_for(&[0xa0], Duration::from_secs(3)).await.is_some();
    if rest > 0 {
        link.collect(Duration::from_secs(rest), Duration::from_secs(rest)).await;
    }
    Ok(s)
}

fn summarise_ppg(report: &Report, sessions: &[PpgSession], battery: &[(i64, u8, u16, u8)], reconnects: usize, failed: usize, stop: &str) {
    let frames: usize = sessions.iter().map(|s| s.frames).sum();
    let streamed = sessions.iter().filter(|s| s.first.is_some()).count();
    let span = match (sessions.first(), sessions.last()) {
        (Some(a), Some(b)) => b.last - a.started,
        _ => 0.0,
    };
    let lengths: Vec<f64> = sessions.iter().filter_map(|s| s.first.map(|f| s.last - f)).collect();
    let median_len = median(lengths.clone()).unwrap_or(0.0);
    let gaps: Vec<f64> = sessions.windows(2).filter_map(|w| Some(w[1].first? - w[0].last)).collect();
    report.finding(
        "Overnight raw PPG",
        format!(
            "{} sessions ({streamed} with frames, stopped: {stop}); {frames} frames = {} samples at 25 Hz; \
             {:.0} % of {:.1} h covered; median session {median_len:.0} s, longest {:.0} s; median gap {:.1} s; \
             reconnects {reconnects}, failed reconnect attempts {failed}",
            sessions.len(),
            frames * 10,
            if span > 0.0 { 100.0 * frames as f64 * 0.4 / span } else { 0.0 },
            span / 3600.0,
            lengths.iter().cloned().fold(0.0, f64::max),
            median(gaps).unwrap_or(0.0),
        ),
    );
    // Two chained runs without 20 01 00 stopped answering 10 to 11 min in.
    let left = sessions.iter().filter(|s| s.left).count();
    let last_frame = sessions.iter().filter(|s| s.first.is_some()).map(|s| s.last).fold(0.0, f64::max);
    report.finding(
        "Overnight raw PPG: factory mode",
        format!(
            "left with 20 01 00 (a0 reply) after {left} of {} sessions; last PPG frame {:.0} min {:02.0} s into the run",
            sessions.len(),
            (last_frame / 60.0).floor(),
            last_frame % 60.0
        ),
    );
    let drain = match (battery.first(), battery.last()) {
        (Some(a), Some(b)) if b.0 > a.0 => {
            let h = (b.0 - a.0) as f64 / 3600.0;
            format!("{} % to {} % over {h:.1} h, {:.1} points an hour", a.1, b.1, (a.1 as f64 - b.1 as f64) / h)
        }
        _ => "not enough readings".into(),
    };
    report.finding("Overnight: battery", drain);
    let mut blines = vec!["clock     battery  mV     state".to_string()];
    for (t, pct, mv, state) in battery {
        blines.push(format!("{}  {:>5} %  {:>5}  {state:02x}", clock(*t), pct, mv));
    }
    report.section("Overnight battery", blines);
    let muted = report.muted().iter().map(|(op, n)| format!("{op:02x} x{n}")).collect::<Vec<_>>().join(", ");
    report.finding("Frames counted, not logged", muted);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spo2_frame_fields() {
        let f = ringconn::frame::parse_hex("15 01 00 00 5d 14 13 04 00 00 8b 1d d6 02 62 00 6a").unwrap();
        assert_eq!(fields(&f), Some((0x0413_145d, 0x02d6_1d8b, 98)));
        assert_eq!(fields(&[0x15, 0x00, 0x44]), None);
    }

    #[test]
    fn statistics() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        let line: Vec<(f64, f64)> = (0..20).map(|i| (i as f64, 2.0 * i as f64)).collect();
        assert!((pearson(&line).unwrap() - 1.0).abs() < 1e-9);
    }
}
