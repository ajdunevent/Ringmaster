//! Ultrahuman rings: clock on connect, battery from the state characteristic, live heart rate
//! through an exercise, and history sync by record number. Protocol notes are in
//! docs/ULTRAHUMAN.md.

use super::{transport::Link, transport::Received, Backend, Command, Event, MEASURE_WINDOW};
use crate::settings::Stat;
use crate::store::Metric;
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::time::Instant;
use ultrahuman::{op, Live, Record, Response, State};

/// Silence that ends a response wait. The ring answers within a fraction of a second.
const QUIET: Duration = Duration::from_secs(4);
/// Requests for the rest of the records when the ring goes quiet before the last one.
const RETRIES: usize = 3;
/// `settings` key holding, per ring, the last record read: its number and its time.
const LAST_READ: &str = "ultrahuman_last_read:";

/// Sets the clock: records are stamped by it, and it drifts by seconds between syncs.
pub async fn greet(link: &mut Link) -> Result<()> {
    link.send(&ultrahuman::set_time(super::unix_now() as u32)).await
}

/// What a records request returned.
struct Read {
    /// Records with their raw bytes, in the order the ring sent them (record-number order).
    records: Vec<(Record, Vec<u8>)>,
    responses: usize,
}

impl Backend {
    /// The response and live-data characteristics. Responses outside a sync (set time, check
    /// data) need no handling.
    pub(super) fn on_ultrahuman_frame(&mut self, frame: &[u8]) {
        let Some(live) = Live::parse(frame) else { return };
        let Some(m) = self.conn.as_mut().and_then(|c| c.measurement.as_mut()) else { return };
        m.last_frame = Instant::now();
        let value = live.reading().then_some(live.heart_rate);
        if let Some(v) = value {
            m.locked.get_or_insert_with(Instant::now);
            m.samples.push(v);
        }
        let progress = m.locked.map_or(0.0, |t| (t.elapsed().as_secs_f64() / MEASURE_WINDOW.as_secs_f64()).min(1.0));
        self.emit(Event::MeasureProgress { value, progress });
    }

    /// The state characteristic. The battery level reads high on the charger and falls back once
    /// the ring is off it, so none is shown while charging.
    pub(super) fn on_ultrahuman_state(&mut self, raw: &[u8]) {
        let Some(s) = State::parse(raw) else { return };
        if s.charging() {
            self.emit(Event::Battery { percent: None, charging: true });
        } else {
            self.emit(Event::Battery { percent: Some(s.battery), charging: false });
            self.alert(Stat::Battery, s.battery as f64, super::unix_now());
        }
    }

    /// Next notification during a sync. Disconnect and Shutdown end the sync; other commands wait.
    async fn ultrahuman_next(&mut self, limit: Duration) -> Result<Received> {
        loop {
            let c = self.conn.as_mut().context("the ring disconnected")?;
            tokio::select! {
                r = c.link.recv(limit) => {
                    match &r {
                        Received::Closed => {
                            self.lost_connection();
                            bail!("the ring disconnected during sync");
                        }
                        Received::Data(state) => {
                            self.last_frame = Instant::now();
                            let state = state.clone();
                            self.on_ultrahuman_state(&state);
                        }
                        Received::Frame(_) => self.last_frame = Instant::now(),
                        Received::Quiet => {}
                    }
                    return Ok(r);
                }
                command = self.commands.recv() => match command {
                    Ok(Command::Shutdown) | Ok(Command::Disconnect) | Err(_) => bail!("sync interrupted"),
                    _ => continue,
                },
            }
        }
    }

    /// Sends a one-response command and returns the response, or `None` if none came.
    async fn ultrahuman_ask(&mut self, command: &[u8]) -> Result<Option<Vec<u8>>> {
        self.conn.as_mut().context("the ring disconnected")?.link.send(command).await?;
        let deadline = Instant::now() + QUIET;
        loop {
            match self.ultrahuman_next(deadline.saturating_duration_since(Instant::now())).await? {
                Received::Frame(f) if f.first() == command.first() && ultrahuman::check_ok(&f) => return Ok(Some(f)),
                Received::Frame(f) => self.on_ultrahuman_frame(&f),
                Received::Data(_) => {}
                Received::Quiet | Received::Closed => return Ok(None),
            }
        }
    }

    async fn ultrahuman_record_number(&mut self, operation: u8) -> Result<u16> {
        let reply = self.ultrahuman_ask(&[operation]).await?.with_context(|| format!("no answer to {}", op::name(operation)))?;
        Response::parse(&reply).and_then(|r| r.record_number()).with_context(|| format!("unexpected {} reply", op::name(operation)))
    }

    /// Every record from `from` up to `last`. The ring streams them after one request; if it goes
    /// quiet first, the rest is asked for again.
    async fn ultrahuman_read(&mut self, from: u16, last: u16, expected: usize) -> Result<Read> {
        let mut read = Read { records: Vec::new(), responses: 0 };
        let mut next = from;
        for _ in 0..=RETRIES {
            self.conn.as_mut().context("the ring disconnected")?.link.send(&ultrahuman::records_from(next)).await?;
            let mut done = false;
            loop {
                let f = match self.ultrahuman_next(QUIET).await? {
                    Received::Frame(f) if f.first() == Some(&op::RECORDS) => f,
                    Received::Frame(f) => {
                        self.on_ultrahuman_frame(&f);
                        continue;
                    }
                    Received::Data(_) => continue,
                    Received::Quiet | Received::Closed => break,
                };
                if !ultrahuman::check_ok(&f) {
                    tracing::info!(frame = %ultrahuman::hex(&f), "Ultrahuman records response with a bad check");
                    break;
                }
                let Some(r) = Response::parse(&f) else { break };
                if r.status == ultrahuman::NOTHING_RECORDED {
                    read.responses += 1;
                    done = true;
                    break;
                }
                if r.status != 0 {
                    bail!("the ring refused the records request (status {:02x})", r.status);
                }
                let count = r.result as usize;
                let chunks: Vec<&[u8]> = r.rest.chunks_exact(ultrahuman::RECORD_LEN).take(count).collect();
                for chunk in &chunks {
                    if let Some(record) = Record::parse(chunk) {
                        next = record.number.wrapping_add(1);
                        read.records.push((record, chunk.to_vec()));
                    }
                }
                read.responses += 1;
                let remaining = expected.saturating_sub(read.records.len()).min(u16::MAX as usize) as u16;
                self.emit(Event::SyncProgress { records: read.records.len(), remaining });
                if chunks.len() < ultrahuman::RECORDS_PER_RESPONSE || read.records.last().is_some_and(|(r, _)| r.number == last) {
                    done = true;
                    break;
                }
            }
            if done {
                break;
            }
            tracing::info!(next, "Ultrahuman ring went quiet before its last record; asking again");
        }
        Ok(read)
    }

    pub(super) async fn sync_ultrahuman(&mut self) -> Result<()> {
        self.emit(Event::SyncStarted);
        self.last_frame = Instant::now();
        let address = self.conn.as_ref().map(|c| c.address.clone()).context("connect to the ring first")?;
        let since = self.store.last_sync(&address)?.unwrap_or(0);
        let result = self.ultrahuman_history(&address, since).await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        tracing::info!(stored = *result.as_ref().unwrap_or(&0), error = error.as_deref().unwrap_or(""), "Ultrahuman sync finished");
        if error.is_none() {
            self.store.mark_synced(&address, super::unix_now())?;
            if let Ok((steps, _)) = self.store.steps_today(&address) {
                let left = (self.settings.step_goal as i64 - steps).max(0);
                self.alert(Stat::DailySteps, steps as f64, super::unix_now());
                self.alert(Stat::StepsToGoal, left as f64, super::unix_now());
            }
        }
        self.emit(Event::SyncFinished { error });
        self.refresh();
        Ok(())
    }

    /// Reads the records not yet stored and stores them; returns how many rows were written.
    async fn ultrahuman_history(&mut self, ring: &str, since: i64) -> Result<usize> {
        if self.ultrahuman_ask(&ultrahuman::set_time(super::unix_now() as u32)).await?.is_none() {
            bail!("the ring did not answer");
        }
        let first = self.ultrahuman_record_number(op::FIRST_RECORD).await?;
        let last = self.ultrahuman_record_number(op::LAST_RECORD).await?;
        // A first number above the last means the numbers wrapped.
        let start = if first > last { 0 } else { first };
        let held = |n: u16| if start <= last { (start..=last).contains(&n) } else { n >= start || n <= last };

        // The last record read is read again, and its time compared: a ring that was reset or
        // ran flat numbers its records from 1 again.
        let key = format!("{LAST_READ}{ring}");
        let previous: Option<(u16, u32)> = self.store.value(&key)?.and_then(|v| {
            let (n, t) = v.split_once(' ')?;
            Some((n.parse().ok()?, t.parse().ok()?))
        });
        let resume = previous.filter(|(n, _)| held(*n));
        let from = resume.map_or(start, |(n, _)| n);
        let expected = last.wrapping_sub(from) as usize + 1;
        let mut read = self.ultrahuman_read(from, last, expected).await?;
        if let Some((n, t)) = resume {
            let same = read.records.first().is_some_and(|(r, _)| r.number == n && r.activity_at == t);
            if !same && from != start {
                tracing::info!("Ultrahuman record numbers started again; reading every record");
                read = self.ultrahuman_read(start, last, last.wrapping_sub(start) as usize + 1).await?;
            }
        }
        if read.responses == 0 {
            bail!("the ring sent no records");
        }

        let rows = rows(&read.records, super::unix_now());
        let mut stored = 0;
        stored += self.store.store_samples(ring, Metric::HeartRate.sample_kind(), &rows.heart_rates)?;
        stored += self.store.store_samples(ring, Metric::Hrv.sample_kind(), &rows.hrvs)?;
        stored += self.store.store_samples(ring, Metric::Spo2.sample_kind(), &rows.spo2)?;
        stored += self.store.store_samples(ring, Metric::SkinTemp.sample_kind(), &rows.skin)?;
        stored += self.store.store_samples(ring, "stress", &rows.stress)?;
        stored += self.store.store_samples(ring, "intensity", &rows.intensity)?;
        stored += self.store.store_steps(ring, &rows.steps)?;
        self.store.store_ultrahuman_records(ring, &rows.raw)?;

        if let Some((r, _)) = read.records.last() {
            self.store.set_setting(&key, &format!("{} {}", r.number, r.activity_at))?;
        }
        for (unix, v) in rows.heart_rates.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::HeartRate, v, unix);
        }
        for (unix, v) in rows.hrvs.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::Hrv, v, unix);
        }
        for (unix, v) in rows.spo2.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::Spo2, v, unix);
        }
        Ok(stored)
    }
}

/// What a sync stores, from the records read.
#[derive(Debug, Default)]
struct Rows {
    heart_rates: Vec<(i64, f64)>,
    hrvs: Vec<(i64, f64)>,
    spo2: Vec<(i64, f64)>,
    skin: Vec<(i64, f64)>,
    stress: Vec<(i64, f64)>,
    intensity: Vec<(i64, f64)>,
    steps: Vec<(i64, i64, f64)>,
    /// (number, time, kind, raw bytes) for every dated record, worn or not.
    raw: Vec<(u16, i64, u8, Vec<u8>)>,
}

/// Dates records stamped before the clock was set, then keeps the readings of worn records. Zero
/// means "not measured" in every field. A record still without a date, or dated in the future,
/// is left out.
fn rows(read: &[(Record, Vec<u8>)], now: i64) -> Rows {
    let mut records: Vec<Record> = read.iter().map(|(r, _)| r.clone()).collect();
    ultrahuman::date_undated(&mut records);
    let mut rows = Rows::default();
    for (r, (_, raw)) in records.iter().zip(read) {
        if r.undated() || r.activity_at as i64 > now + 60 {
            continue;
        }
        rows.raw.push((r.number, r.activity_at as i64, r.kind, raw.clone()));
        if !r.worn() {
            continue;
        }
        let (optical, activity) = (r.optical_at as i64, r.activity_at as i64);
        if r.heart_rate > 0 {
            rows.heart_rates.push((optical, r.heart_rate as f64));
        }
        if r.hrv > 0 {
            rows.hrvs.push((optical, r.hrv as f64));
        }
        if r.spo2 > 0 {
            rows.spo2.push((optical, r.spo2 as f64));
        }
        if r.temperature_max > 0.0 {
            rows.skin.push((r.temperature_at as i64, r.temperature_max as f64));
        }
        if r.stress > 0 {
            rows.stress.push((activity, r.stress as f64));
        }
        rows.intensity.push((activity, r.intensity as f64));
        rows.steps.push((activity, r.steps as i64, 0.0));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(h: &str) -> Vec<u8> {
        let h: String = h.split_whitespace().collect();
        (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn stores_worn_readings_and_dates_early_records() {
        // From a Ring AIR (firmware 02.00.08.53): two records stamped before its clock was set,
        // one not worn after it, and two exercise records.
        let raw = [
            "34 01 00 00 00 00 00 6e 34 01 00 00 00 00 00 00 00 00 00 00 34 01 00 00 00 00 00 00 00 00 01 00",
            "60 02 00 00 00 00 00 6e 60 02 00 00 00 00 00 00 00 00 00 00 60 02 00 00 00 00 00 00 00 00 02 00",
            "cf 8f f8 6a 00 00 00 6e cf 8f f8 6a 00 00 00 00 00 00 00 00 cf 8f f8 6a 00 00 00 00 00 00 03 00",
            "d5 90 f8 6a 45 00 00 05 d5 90 f8 6a 05 fd f5 41 da 79 f1 41 d5 90 f8 6a 00 00 00 00 00 00 04 00",
            "e1 90 f8 6a 39 28 00 05 e1 90 f8 6a e5 59 08 42 7c 38 06 42 e1 90 f8 6a 00 00 00 00 28 00 0a 00",
        ];
        let read: Vec<(Record, Vec<u8>)> = raw.iter().map(|h| (Record::parse(&hex(h)).unwrap(), hex(h))).collect();
        let rows = rows(&read, 0x6af9_0000);
        let at = 0x6af8_8fcf_i64;
        assert_eq!(
            rows.raw.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            [(1, at - 600), (2, at - 300), (3, at), (4, 0x6af8_90d5), (10, 0x6af8_90e1)]
        );
        assert_eq!(rows.heart_rates, [(0x6af8_90d5, 69.0), (0x6af8_90e1, 57.0)]);
        assert_eq!(rows.hrvs, [(0x6af8_90e1, 40.0)]);
        assert!(rows.spo2.is_empty());
        assert_eq!(rows.skin.len(), 2);
        assert!((rows.skin[1].1 - 34.09).abs() < 0.01);
        // Not-worn records add no steps or intensity.
        assert_eq!(rows.steps.len(), 2);
        // Nothing from the future.
        assert!(super::rows(&read, at - 1000).raw.is_empty());
    }
}
