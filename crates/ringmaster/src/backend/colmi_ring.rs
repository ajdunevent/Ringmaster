//! Colmi rings: setup on connect, live readings, battery and history sync. Protocol notes are in
//! docs/COLMI.md.

use super::{transport::Link, transport::Received, Backend, Command, Event, MEASURE_WINDOW};
use crate::settings::Stat;
use crate::store::Metric;
use anyhow::{bail, Context, Result};
use chrono::{Duration as Days, Local, NaiveDate, NaiveDateTime, TimeZone};
use colmi::bigdata::{self, Assembler};
use colmi::history::{ActivityDay, HalfHourly, HeartRateDay, Step};
use colmi::live::Reading;
use colmi::{packet, request};
use std::time::Duration;
use tokio::time::Instant;

/// How often a running live reading is told to continue.
pub const KEEP_GOING: Duration = Duration::from_secs(8);
/// Automatic heart rate interval set on every connection, in minutes.
const HEART_RATE_EVERY: u8 = 10;
/// Days of history asked for; the rings keep about a week.
const DAYS: u8 = 7;
/// Silence that ends a history answer; a day without data can go unanswered.
const QUIET: Duration = Duration::from_secs(4);
const BIG_QUIET: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_millis(150);

/// Sets the clock, which history is dated by, and turns on the automatic measurements.
pub async fn greet(link: &mut Link) -> Result<()> {
    link.send(&request::set_time(Local::now().naive_local())).await?;
    let setup = [
        request::phone_name(),
        request::heart_rate_interval(Some(HEART_RATE_EVERY)),
        request::spo2_auto(true),
        request::hrv_auto(true),
        request::stress_auto(true),
        request::temperature_auto(true),
        request::battery(),
    ];
    for packet in setup {
        tokio::time::sleep(SETTLE).await;
        link.send(&packet).await?;
    }
    Ok(())
}

fn unix(t: NaiveDateTime) -> Option<i64> {
    Local.from_local_datetime(&t).earliest().map(|t| t.timestamp())
}

fn at_minute(day: NaiveDate, minute: u16) -> Option<i64> {
    unix(day.and_hms_opt(0, 0, 0)? + Days::minutes(minute as i64))
}

impl Backend {
    pub(super) fn on_colmi_frame(&mut self, frame: &[u8]) {
        if !packet::valid(frame) {
            tracing::debug!(frame = %packet::hex(frame), "Colmi packet with a bad checksum");
            return;
        }
        if let Some((percent, charging)) = colmi::live::battery(frame) {
            self.emit(Event::Battery { percent, charging });
            self.alert(Stat::Battery, percent as f64, super::unix_now());
            return;
        }
        let Some(reading) = colmi::live::reading(frame) else { return };
        let Some(m) = self.conn.as_mut().and_then(|c| c.measurement.as_mut()) else { return };
        m.last_frame = Instant::now();
        let value = match reading {
            Reading::Value(kind, v) if kind == super::colmi_kind(m.kind) => Some(v),
            _ => None,
        };
        if let Some(v) = value {
            m.locked.get_or_insert_with(Instant::now);
            m.samples.push(v);
        }
        let progress = m.locked.map_or(0.0, |t| (t.elapsed().as_secs_f64() / MEASURE_WINDOW.as_secs_f64()).min(1.0));
        self.emit(Event::MeasureProgress { value, progress });
    }

    /// Next frame for a history request. Disconnect and Shutdown end the sync; other commands wait.
    async fn colmi_next(&mut self, limit: Duration) -> Result<Received> {
        loop {
            let c = self.conn.as_mut().context("the ring disconnected")?;
            tokio::select! {
                r = c.link.recv(limit) => {
                    if let Received::Closed = r {
                        self.lost_connection();
                        bail!("the ring disconnected during sync");
                    }
                    if matches!(r, Received::Frame(_) | Received::Data(_)) {
                        self.last_frame = Instant::now();
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

    /// Sends a history request and feeds the answer to `parse`. `None` when the ring has nothing
    /// or stays quiet.
    async fn colmi_ask<T>(&mut self, request: &[u8], mut parse: impl FnMut(&[u8]) -> Step<T>) -> Result<Option<T>> {
        self.conn.as_mut().context("the ring disconnected")?.link.send(request).await?;
        loop {
            match self.colmi_next(QUIET).await? {
                Received::Frame(f) => {
                    if !packet::valid(&f) {
                        continue;
                    }
                    match parse(&f) {
                        Step::Done(v) => return Ok(Some(v)),
                        Step::Empty => return Ok(None),
                        Step::More => self.on_colmi_frame(&f),
                    }
                }
                Received::Data(_) => {}
                Received::Quiet | Received::Closed => return Ok(None),
            }
        }
    }

    async fn colmi_big(&mut self, request: &[u8], kind: u8) -> Result<Option<Vec<u8>>> {
        self.conn.as_mut().context("the ring disconnected")?.link.send_data(request).await?;
        let mut assembler = Assembler::default();
        loop {
            match self.colmi_next(BIG_QUIET).await? {
                Received::Data(f) => match assembler.feed(&f) {
                    Some(p) if p.kind == kind && p.crc_ok => return Ok(Some(p.payload)),
                    Some(p) if p.kind == kind => {
                        tracing::info!(kind, "Colmi big-data packet with a bad CRC");
                        return Ok(None);
                    }
                    _ => {}
                },
                Received::Frame(f) => self.on_colmi_frame(&f),
                Received::Quiet | Received::Closed => return Ok(None),
            }
        }
    }

    pub(super) async fn sync_colmi(&mut self) -> Result<()> {
        self.emit(Event::SyncStarted);
        self.last_frame = Instant::now();
        let address = self.conn.as_ref().map(|c| c.address.clone()).context("connect to the ring first")?;
        let since = self.store.last_sync(&address)?.unwrap_or(0);
        let result = self.colmi_history(&address, since).await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        tracing::info!(stored = *result.as_ref().unwrap_or(&0), error = error.as_deref().unwrap_or(""), "Colmi sync finished");
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

    /// Asks for every kind of history and stores it; returns how many rows were written.
    async fn colmi_history(&mut self, ring: &str, since: i64) -> Result<usize> {
        let today = Local::now().date_naive();
        let now = super::unix_now();
        let big = self.conn.as_ref().is_some_and(|c| c.link.has_data());
        let total = 3 * DAYS as u16 + 1 + if big { 3 } else { 0 };
        let mut done = 0u16;
        let mut answered = 0;
        let mut stored = 0;
        let progress = |done: &mut u16, this: &Backend| {
            *done += 1;
            this.emit(Event::SyncProgress { records: *done as usize, remaining: total.saturating_sub(*done) });
        };

        for days_ago in (0..DAYS).rev() {
            let mut parser = ActivityDay::default();
            if let Some(quarters) = self.colmi_ask(&request::activity(days_ago), |p| parser.feed(p)).await? {
                answered += 1;
                let rows: Vec<(i64, i64, f64)> =
                    quarters.iter().filter_map(|q| Some((unix(q.start()?)?, q.steps as i64, q.distance as f64))).collect();
                stored += self.store.store_steps(ring, &rows)?;
            }
            progress(&mut done, self);
        }

        let mut heart_rates = Vec::new();
        let mut hrvs = Vec::new();
        for days_ago in (0..DAYS).rev() {
            let day = today - Days::days(days_ago as i64);
            let mut parser = HeartRateDay::default();
            if let Some(values) = self.colmi_ask(&request::heart_rate(day), |p| parser.feed(p)).await? {
                answered += 1;
                heart_rates.extend(values.into_iter().filter_map(|(m, v)| Some((at_minute(day, m)?, v as f64))));
            }
            progress(&mut done, self);
            let mut parser = HalfHourly::hrv();
            if let Some(values) = self.colmi_ask(&request::hrv(days_ago), |p| parser.feed(p)).await? {
                answered += 1;
                hrvs.extend(values.into_iter().filter_map(|(m, v)| Some((at_minute(day, m)?, v as f64))));
            }
            progress(&mut done, self);
        }
        // Today's answer has slots for the rest of the day.
        heart_rates.retain(|p| p.0 <= now);
        hrvs.retain(|p| p.0 <= now);
        stored += self.store.store_samples(ring, Metric::HeartRate.sample_kind(), &heart_rates)?;
        stored += self.store.store_samples(ring, Metric::Hrv.sample_kind(), &hrvs)?;

        let mut parser = HalfHourly::stress();
        if let Some(values) = self.colmi_ask(&request::stress(), |p| parser.feed(p)).await? {
            answered += 1;
            let points: Vec<(i64, f64)> =
                values.into_iter().filter_map(|(m, v)| Some((at_minute(today, m)?, v as f64))).filter(|p| p.0 <= now).collect();
            stored += self.store.store_samples(ring, "stress", &points)?;
        }
        progress(&mut done, self);

        let mut spo2 = Vec::new();
        if big {
            if let Some(payload) = self.colmi_big(&request::spo2(), bigdata::SPO2).await? {
                answered += 1;
                let mut lowest = Vec::new();
                for (days_ago, hour, lo, hi) in bigdata::spo2(&payload) {
                    let day = today - Days::days(days_ago as i64);
                    let Some(t) = at_minute(day, hour as u16 * 60) else { continue };
                    // The ring keeps each hour's range; its middle stands in for the hour.
                    spo2.push((t, (lo as f64 + hi as f64) / 2.0));
                    lowest.push((t, lo as f64));
                }
                spo2.retain(|p| p.0 <= now);
                lowest.retain(|p| p.0 <= now);
                stored += self.store.store_samples(ring, Metric::Spo2.sample_kind(), &spo2)?;
                stored += self.store.store_samples(ring, "spo2_low", &lowest)?;
            }
            progress(&mut done, self);

            if let Some(payload) = self.colmi_big(&request::temperature(), bigdata::TEMPERATURE).await? {
                answered += 1;
                let points: Vec<(i64, f64)> = bigdata::temperature(&payload)
                    .into_iter()
                    .filter_map(|(days_ago, minute, c)| Some((at_minute(today - Days::days(days_ago as i64), minute)?, c as f64)))
                    .filter(|p| p.0 <= now)
                    .collect();
                stored += self.store.store_samples(ring, Metric::SkinTemp.sample_kind(), &points)?;
            }
            progress(&mut done, self);

            if let Some(payload) = self.colmi_big(&request::sleep(), bigdata::SLEEP).await? {
                answered += 1;
                let mut nights = Vec::new();
                for session in bigdata::sleep(&payload) {
                    let Some((start, end)) = session.span(today) else { continue };
                    let (Some(start), Some(end)) = (unix(start), unix(end)) else { continue };
                    let mut at = start;
                    let mut stages = Vec::new();
                    for (stage, minutes) in &session.stages {
                        stages.push((at, *minutes as i64, stage.code()));
                        at += *minutes as i64 * 60;
                    }
                    nights.push((start, end, stages));
                }
                self.store.store_ring_sleep(ring, &nights)?;
                stored += nights.len();
            }
            progress(&mut done, self);
        }

        if answered == 0 {
            bail!("the ring did not answer any history request");
        }
        for (unix, v) in heart_rates.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::HeartRate, v, unix);
        }
        for (unix, v) in hrvs.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::Hrv, v, unix);
        }
        for (unix, v) in spo2.into_iter().filter(|p| p.0 > since) {
            self.alert(Stat::Spo2, v, unix);
        }
        Ok(stored)
    }
}
