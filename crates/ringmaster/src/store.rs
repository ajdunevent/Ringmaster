//! Local SQLite store. Raw record bytes are kept beside the decoded columns so history can be
//! re-decoded as the protocol is better understood.

use crate::settings::{Op, Rule, Settings, Stat};
use anyhow::{Context, Result};
use ringconn::drain::Batch;
use ringconn::records::{Descriptor, EpochLayout};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Applied in order; `PRAGMA user_version` counts how many have run.
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE rings (
    address   TEXT PRIMARY KEY,
    name      TEXT,
    serial    TEXT,
    firmware  TEXT,
    hardware  TEXT,
    last_sync INTEGER
);
CREATE TABLE epochs (
    ring       TEXT NOT NULL,
    counter    INTEGER NOT NULL,
    unix       INTEGER NOT NULL,
    channel    INTEGER NOT NULL,
    layout     TEXT NOT NULL,
    heart_rate INTEGER,
    hrv        INTEGER,
    spo2       INTEGER,
    resp_rate  REAL,
    confidence INTEGER NOT NULL,
    raw        BLOB NOT NULL,
    PRIMARY KEY (ring, counter)
);
CREATE INDEX epochs_time ON epochs (ring, unix);
CREATE TABLE optical (
    ring    TEXT NOT NULL,
    counter INTEGER NOT NULL,
    unix    INTEGER NOT NULL,
    raw     BLOB NOT NULL,
    PRIMARY KEY (ring, counter)
);
CREATE TABLE sport (
    ring       TEXT NOT NULL,
    counter    INTEGER NOT NULL,
    unix       INTEGER NOT NULL,
    heart_rate INTEGER,
    steps      INTEGER NOT NULL,
    raw        BLOB NOT NULL,
    PRIMARY KEY (ring, counter)
);
CREATE TABLE telemetry (
    ring        TEXT NOT NULL,
    unix        INTEGER NOT NULL,
    battery     INTEGER NOT NULL,
    state       INTEGER NOT NULL,
    steps       INTEGER NOT NULL,
    skin_temp_1 REAL NOT NULL,
    skin_temp_2 REAL NOT NULL,
    millivolts  INTEGER NOT NULL,
    case_byte   INTEGER NOT NULL,
    raw         BLOB NOT NULL
);
CREATE INDEX telemetry_time ON telemetry (ring, unix);
CREATE TABLE event_log (
    ring TEXT NOT NULL,
    unix INTEGER NOT NULL,
    raw  BLOB NOT NULL
);
CREATE TABLE syncs (
    ring     TEXT NOT NULL,
    started  INTEGER NOT NULL,
    finished INTEGER NOT NULL,
    channel  INTEGER NOT NULL,
    outcome  TEXT NOT NULL,
    pages    INTEGER NOT NULL,
    records  INTEGER NOT NULL
);
",
    "
CREATE TABLE spot (
    ring    TEXT NOT NULL,
    unix    INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    value   REAL NOT NULL,
    samples INTEGER NOT NULL
);
CREATE INDEX spot_time ON spot (ring, unix);
",
    "
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE notification_rules (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    stat      TEXT NOT NULL,
    op        TEXT NOT NULL,
    threshold REAL NOT NULL
);
INSERT INTO notification_rules (stat, op, threshold) VALUES ('battery', '<', 5), ('hours_since_sync', '>', 6);
",
    "ALTER TABLE rings ADD COLUMN nickname TEXT;",
    "ALTER TABLE spot ADD COLUMN low REAL; ALTER TABLE spot ADD COLUMN high REAL;",
    "DELETE FROM spot WHERE rowid NOT IN (SELECT MAX(rowid) FROM spot GROUP BY ring, kind);",
    // The untouched default of 6 hours becomes the new default of 5 days.
    "
UPDATE notification_rules
SET stat = 'days_since_sync', threshold = CASE WHEN threshold = 6 THEN 5 ELSE MIN(7, MAX(1, ROUND(threshold / 24.0))) END
WHERE stat = 'hours_since_sync';
CREATE TABLE steps (
    ring     TEXT NOT NULL,
    unix     INTEGER NOT NULL,
    steps    INTEGER NOT NULL,
    distance REAL,
    PRIMARY KEY (ring, unix)
);
CREATE TABLE official (
    ring TEXT NOT NULL,
    kind TEXT NOT NULL,
    unix INTEGER,
    data TEXT NOT NULL,
    UNIQUE (ring, kind, data)
);
CREATE INDEX official_time ON official (ring, kind, unix);
",
    // Readings from rings whose history comes as values rather than RingConn records.
    "
ALTER TABLE rings ADD COLUMN brand TEXT NOT NULL DEFAULT 'ringconn';
CREATE TABLE samples (
    ring  TEXT NOT NULL,
    kind  TEXT NOT NULL,
    unix  INTEGER NOT NULL,
    value REAL NOT NULL,
    PRIMARY KEY (ring, kind, unix)
);
CREATE TABLE ring_sleep (
    ring  TEXT NOT NULL,
    start INTEGER NOT NULL,
    end   INTEGER NOT NULL,
    PRIMARY KEY (ring, start)
);
CREATE TABLE sleep_stages (
    ring    TEXT NOT NULL,
    start   INTEGER NOT NULL,
    minutes INTEGER NOT NULL,
    stage   INTEGER NOT NULL,
    PRIMARY KEY (ring, start)
);
",
    // A forgotten ring keeps its history but is never searched for.
    "ALTER TABLE rings ADD COLUMN forgotten INTEGER NOT NULL DEFAULT 0;",
];

/// A night scored by the ring: start, end, and stages as (start, minutes, stage code).
pub type RingNight = (i64, i64, Vec<(i64, i64, u8)>);

/// `rings.brand` values.
pub const RINGCONN: &str = "ringconn";
pub const COLMI: &str = "colmi";

/// `sleep_stages.stage` for time awake, as the Colmi ring numbers it.
const STAGE_AWAKE: i64 = 5;

/// `epochs.channel` for records copied from the official app.
pub const IMPORTED_CHANNEL: u8 = 0xff;

/// A skin temperature sample needs a worn epoch at most this far away: one epoch's length.
const WORN_NEAR: i64 = 150;

/// Readings outside this range are not skin temperature.
const SKIN_TEMP_RANGE: std::ops::RangeInclusive<f64> = 10.0..=45.0;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, Default)]
pub struct RingInfo {
    pub address: String,
    pub name: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub hardware: Option<String>,
    /// Set in Ringmaster; `upsert_ring` leaves it alone.
    pub nickname: Option<String>,
    /// `RINGCONN` or `COLMI`; `None` keeps the stored value.
    pub brand: Option<String>,
}

impl RingInfo {
    pub fn display_name(&self) -> String {
        self.nickname.clone().or_else(|| self.name.clone()).unwrap_or_else(|| self.address.clone())
    }
}

/// A ring as listed in Preferences.
#[derive(Debug, Clone)]
pub struct RingEntry {
    pub info: RingInfo,
    pub last_sync: Option<i64>,
    pub forgotten: bool,
}

/// Settings key holding the address of the ring chosen in Ringmaster.
const ACTIVE_RING: &str = "active_ring";

/// One channel's drain.
#[derive(Debug, Clone)]
pub struct SyncRecord {
    pub started: i64,
    pub finished: i64,
    pub channel: u8,
    pub outcome: String,
    pub pages: usize,
    pub records: usize,
}

/// A value with the time it was recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading<T> {
    pub value: T,
    pub unix: i64,
}

/// Reads value, samples, unix, low and high starting at column `at`.
fn spot_from(r: &rusqlite::Row, at: usize) -> rusqlite::Result<Spot> {
    let (low, high): (Option<f64>, Option<f64>) = (r.get(at + 3)?, r.get(at + 4)?);
    Ok(Spot { value: r.get(at)?, samples: r.get(at + 1)?, unix: r.get(at + 2)?, range: low.zip(high) })
}

/// Returns 1 for a new row, 0 for one already stored.
pub fn insert_epoch(tx: &rusqlite::Transaction, ring: &str, channel: u8, e: &ringconn::records::Epoch) -> Result<usize> {
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO epochs (ring, counter, unix, channel, layout, heart_rate, hrv, spo2, resp_rate, confidence, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let layout = match e.layout() {
        EpochLayout::Idle => "idle",
        EpochLayout::Activity => "activity",
        EpochLayout::SleepVitals => "sleep-vitals",
    };
    Ok(stmt.execute(params![
        ring,
        e.counter,
        ringconn::time::to_unix(e.counter),
        channel,
        layout,
        e.heart_rate(),
        e.hrv(),
        e.spo2(),
        e.respiratory_rate(),
        e.confidence(),
        &e.raw[..],
    ])?)
}

pub fn local_midnight(day: chrono::NaiveDate) -> i64 {
    use chrono::TimeZone;
    chrono::Local.from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub ring: Option<RingInfo>,
    pub last_sync: Option<i64>,
    pub heart_rate: Option<Reading<i64>>,
    pub hrv: Option<Reading<i64>>,
    pub spo2: Option<Reading<i64>>,
    pub resp_rate: Option<Reading<f64>>,
    pub telemetry: Option<(i64, Descriptor)>,
    /// Newest skin temperature from history.
    pub skin_temp: Option<Reading<f64>>,
    /// Last status frame sent from the charging case: when, case battery %, plugged in.
    pub case: Option<(i64, u8, bool)>,
    pub spot_heart_rate: Option<Spot>,
    pub spot_spo2: Option<Spot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureKind {
    HeartRate,
    Spo2,
}

impl MeasureKind {
    fn key(self) -> &'static str {
        match self {
            MeasureKind::HeartRate => "heart_rate",
            MeasureKind::Spo2 => "spo2",
        }
    }
}

/// A measurement taken on request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    pub value: f64,
    pub samples: i64,
    pub unix: i64,
    /// Lowest and highest reading; absent for measurements saved before version 5 of the schema.
    pub range: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepSource {
    /// The ring's own activity log.
    Ring,
    /// Status frames received while connected.
    Connected,
}

/// A history stream that can be charted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    HeartRate,
    Hrv,
    Spo2,
    RespRate,
    /// From the ring's per-minute history and from status frames while connected.
    SkinTemp,
}

impl Metric {
    pub const ALL: [Metric; 5] = [Metric::HeartRate, Metric::Hrv, Metric::Spo2, Metric::RespRate, Metric::SkinTemp];
    const EPOCH: [Metric; 4] = [Metric::HeartRate, Metric::Hrv, Metric::Spo2, Metric::RespRate];

    /// `samples.kind` for this stream.
    pub fn sample_kind(self) -> &'static str {
        match self {
            Metric::HeartRate => "heart_rate",
            Metric::Hrv => "hrv",
            Metric::Spo2 => "spo2",
            Metric::RespRate => "resp_rate",
            Metric::SkinTemp => "skin_temp",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Metric::HeartRate => "heart_rate",
            Metric::Hrv => "hrv",
            Metric::Spo2 => "spo2",
            Metric::RespRate => "resp_rate",
            Metric::SkinTemp => "MAX(skin_temp_1, skin_temp_2)",
        }
    }

    /// Decimal places shown.
    pub fn precision(self) -> usize {
        match self {
            Metric::RespRate | Metric::SkinTemp => 1,
            _ => 0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Metric::HeartRate => "Heart rate",
            Metric::Hrv => "Heart rate variability",
            Metric::Spo2 => "Blood oxygen",
            Metric::RespRate => "Respiratory rate",
            Metric::SkinTemp => "Skin temperature",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Metric::HeartRate => "HR",
            Metric::Hrv => "HRV",
            Metric::Spo2 => "SpO₂",
            Metric::RespRate => "Resp",
            Metric::SkinTemp => "Temp",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Metric::HeartRate => "bpm",
            Metric::Hrv => "ms",
            Metric::Spo2 => "%",
            Metric::RespRate => "breaths/min",
            Metric::SkinTemp => "°C",
        }
    }

    /// CSS class and chart colour.
    pub fn style(self) -> (&'static str, (f64, f64, f64)) {
        match self {
            Metric::HeartRate => ("metric-hr", (0.878, 0.106, 0.141)),
            Metric::Hrv => ("metric-hrv", (0.569, 0.255, 0.675)),
            Metric::Spo2 => ("metric-spo2", (0.208, 0.518, 0.894)),
            Metric::RespRate => ("metric-resp", (0.180, 0.761, 0.494)),
            Metric::SkinTemp => ("metric-temp", (1.0, 0.471, 0.0)),
        }
    }
}

/// Which kinds of data exist on one local day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayMarks {
    pub metrics: Vec<Metric>,
    pub workout: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

impl Stats {
    pub fn of(values: impl Iterator<Item = f64>) -> Option<Self> {
        let (mut count, mut sum, mut min, mut max) = (0usize, 0.0, f64::MAX, f64::MIN);
        for v in values {
            count += 1;
            sum += v;
            min = min.min(v);
            max = max.max(v);
        }
        (count > 0).then(|| Stats { count, min, max, mean: sum / count as f64 })
    }
}

/// Lowest 30-minute average heart rate; `None` without a full half hour of readings.
pub fn resting_heart_rate(points: &[(i64, f64)]) -> Option<f64> {
    const WINDOW: i64 = 30 * 60;
    let mut best: Option<f64> = None;
    let mut start = 0;
    let mut sum = 0.0;
    for (end, &(t, v)) in points.iter().enumerate() {
        sum += v;
        while t - points[start].0 >= WINDOW {
            sum -= points[start].1;
            start += 1;
        }
        let n = end + 1 - start;
        // Epochs are 2.5 min apart, so a full window holds 12; 10 allows a couple of gaps.
        if n >= 10 {
            let mean = sum / n as f64;
            best = Some(best.map_or(mean, |b: f64| b.min(mean)));
        }
    }
    best
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // synchronous=FULL makes each committed page survive a power loss before its ACK is sent.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        // The UI and the backend each open the database; the immediate transaction serialises
        // their migrations.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version = tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
        if version > MIGRATIONS.len() {
            anyhow::bail!("database schema version {version} is newer than this version of Ringmaster");
        }
        for migration in &MIGRATIONS[version..] {
            tx.execute_batch(migration)?;
        }
        tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
        tx.commit()?;
        Ok(Self { conn })
    }

    pub fn upsert_ring(&self, info: &RingInfo) -> Result<()> {
        self.conn.execute(
            "INSERT INTO rings (address, name, serial, firmware, hardware, brand) VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, 'ringconn'))
             ON CONFLICT (address) DO UPDATE SET
               name = COALESCE(excluded.name, name), serial = COALESCE(excluded.serial, serial),
               firmware = COALESCE(excluded.firmware, firmware), hardware = COALESCE(excluded.hardware, hardware),
               brand = COALESCE(?6, brand), forgotten = 0",
            params![info.address, info.name, info.serial, info.firmware, info.hardware, info.brand],
        )?;
        Ok(())
    }

    pub fn ring(&self, address: &str) -> Result<Option<RingInfo>> {
        Ok(self.summary(address)?.ring)
    }

    /// An empty or blank name clears the nickname.
    pub fn set_nickname(&self, address: &str, nickname: &str) -> Result<()> {
        let nickname = Some(nickname.trim()).filter(|n| !n.is_empty());
        self.conn.execute("UPDATE rings SET nickname = ?2 WHERE address = ?1", params![address, nickname])?;
        Ok(())
    }

    /// Commits the batch in its own transaction; returns the number of new rows.
    pub fn store_batch(&mut self, ring: &str, channel: u8, unix_now: i64, batch: &Batch) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut added = 0;
        match batch {
            Batch::Epochs(epochs) => {
                for e in epochs {
                    added += insert_epoch(&tx, ring, channel, e)?;
                }
            }
            Batch::Optical(records) => {
                let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO optical (ring, counter, unix, raw) VALUES (?1, ?2, ?3, ?4)")?;
                for r in records {
                    added += stmt.execute(params![ring, r.counter, ringconn::time::to_unix(r.counter), r.raw])?;
                }
            }
            Batch::Sport(records) => {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO sport (ring, counter, unix, heart_rate, steps, raw) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for r in records {
                    added +=
                        stmt.execute(params![ring, r.counter, ringconn::time::to_unix(r.counter), r.heart_rate(), r.steps(), &r.raw[..]])?;
                }
            }
            Batch::EventLog(raw) => {
                added += tx.execute("INSERT INTO event_log (ring, unix, raw) VALUES (?1, ?2, ?3)", params![ring, unix_now, raw])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    pub fn store_telemetry(&self, ring: &str, unix: i64, d: &Descriptor, raw: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO telemetry (ring, unix, battery, state, steps, skin_temp_1, skin_temp_2, millivolts, case_byte, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![ring, unix, d.battery, d.state, d.steps, d.skin_temp[0], d.skin_temp[1], d.millivolts, d.case, raw],
        )?;
        Ok(())
    }

    pub fn record_sync(&self, ring: &str, sync: &SyncRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO syncs (ring, started, finished, channel, outcome, pages, records) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ring, sync.started, sync.finished, sync.channel, sync.outcome, sync.pages as i64, sync.records as i64],
        )?;
        Ok(())
    }

    /// Records a sync in which every channel finished without error.
    pub fn mark_synced(&self, ring: &str, unix: i64) -> Result<()> {
        self.conn.execute("UPDATE rings SET last_sync = ?2 WHERE address = ?1", params![ring, unix])?;
        Ok(())
    }

    pub fn last_sync(&self, ring: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT last_sync FROM rings WHERE address = ?1", [ring], |r| r.get(0)).optional()?.flatten())
    }

    pub fn settings(&self) -> Result<Settings> {
        let mut stmt = self.conn.prepare_cached("SELECT key, value FROM settings")?;
        let pairs = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok(Settings::from_pairs(&pairs))
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        for (key, value) in settings.to_pairs() {
            self.set_value(key, &value)?;
        }
        Ok(())
    }

    fn set_value(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn rules(&self) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, stat, op, threshold FROM notification_rules ORDER BY id")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?)))?;
        let mut rules = Vec::new();
        for row in rows {
            let (id, stat, op, threshold) = row?;
            if let (Some(stat), Some(op)) = (Stat::from_key(&stat), Op::from_symbol(&op)) {
                rules.push(Rule { id, stat, op, threshold });
            }
        }
        Ok(rules)
    }

    pub fn add_rule(&self, stat: Stat, op: Op, threshold: f64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notification_rules (stat, op, threshold) VALUES (?1, ?2, ?3)",
            params![stat.key(), op.symbol(), threshold],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_rule(&self, rule: &Rule) -> Result<()> {
        self.conn.execute(
            "UPDATE notification_rules SET stat = ?2, op = ?3, threshold = ?4 WHERE id = ?1",
            params![rule.id, rule.stat.key(), rule.op.symbol(), rule.threshold],
        )?;
        Ok(())
    }

    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM notification_rules WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The ring chosen in Ringmaster, or else the most recently synced ring. It can be forgotten.
    pub fn default_ring(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT address FROM rings
                 ORDER BY address = (SELECT value FROM settings WHERE key = ?1) DESC, COALESCE(last_sync, 0) DESC LIMIT 1",
                [ACTIVE_RING],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The ring Ringmaster connects to: the default ring unless it has been forgotten.
    pub fn ring_to_connect(&self) -> Result<Option<String>> {
        Ok(self.default_ring()?.filter(|a| !self.is_forgotten(a).unwrap_or(true)))
    }

    pub fn set_default_ring(&self, address: &str) -> Result<()> {
        self.set_value(ACTIVE_RING, address)
    }

    pub fn forget_ring(&self, address: &str) -> Result<()> {
        self.conn.execute("UPDATE rings SET forgotten = 1 WHERE address = ?1", [address])?;
        Ok(())
    }

    pub fn is_forgotten(&self, address: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT forgotten FROM rings WHERE address = ?1", [address], |r| r.get::<_, bool>(0))
            .optional()?
            .unwrap_or(false))
    }

    /// Every ring with history here, most recently synced first.
    pub fn rings(&self) -> Result<Vec<RingEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT address, name, serial, firmware, hardware, nickname, brand, last_sync, forgotten FROM rings
             ORDER BY COALESCE(last_sync, 0) DESC, address",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RingEntry {
                info: RingInfo {
                    address: r.get(0)?,
                    name: r.get(1)?,
                    serial: r.get(2)?,
                    firmware: r.get(3)?,
                    hardware: r.get(4)?,
                    nickname: r.get(5)?,
                    brand: r.get(6)?,
                },
                last_sync: r.get(7)?,
                forgotten: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest value from either RingConn records or samples; `column` is also the sample kind.
    fn latest(&self, ring: &str, column: &str) -> Result<Option<Reading<f64>>> {
        let sql = format!(
            "SELECT value, unix FROM (
               SELECT {column} AS value, unix FROM epochs WHERE ring = ?1 AND {column} IS NOT NULL
               UNION ALL SELECT value, unix FROM samples WHERE ring = ?1 AND kind = ?2
             ) ORDER BY unix DESC LIMIT 1"
        );
        Ok(self.conn.query_row(&sql, params![ring, column], |r| Ok(Reading { value: r.get(0)?, unix: r.get(1)? })).optional()?)
    }

    /// Samples can hold fractions, such as the middle of an hourly SpO2 range.
    fn latest_whole(&self, ring: &str, column: &str) -> Result<Option<Reading<i64>>> {
        Ok(self.latest(ring, column)?.map(|r| Reading { value: r.value.round() as i64, unix: r.unix }))
    }

    pub fn brand(&self, ring: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT brand FROM rings WHERE address = ?1", [ring], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| RINGCONN.to_string()))
    }

    /// Stores or updates readings of one kind (a `Metric::column` name, or another kind such as
    /// "stress"); returns how many rows changed.
    pub fn store_samples(&mut self, ring: &str, kind: &str, points: &[(i64, f64)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare_cached("INSERT OR REPLACE INTO samples (ring, kind, unix, value) VALUES (?1, ?2, ?3, ?4)")?;
            for (unix, value) in points {
                n += stmt.execute(params![ring, kind, unix, value])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Quarter-hour step counts: (start, steps, metres). Counts for the current quarter grow, so
    /// later values replace earlier ones.
    pub fn store_steps(&mut self, ring: &str, quarters: &[(i64, i64, f64)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare_cached("INSERT OR REPLACE INTO steps (ring, unix, steps, distance) VALUES (?1, ?2, ?3, ?4)")?;
            for (unix, steps, distance) in quarters {
                n += stmt.execute(params![ring, unix, steps, distance])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Sleep scored by the ring: (start, end, stages as (start, minutes, stage code)). A night
    /// replaces any stored night it overlaps, since the ring can rescore one.
    pub fn store_ring_sleep(&mut self, ring: &str, nights: &[RingNight]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (start, end, stages) in nights {
            tx.execute("DELETE FROM ring_sleep WHERE ring = ?1 AND start < ?3 AND end > ?2", params![ring, start, end])?;
            tx.execute("DELETE FROM sleep_stages WHERE ring = ?1 AND start >= ?2 AND start < ?3", params![ring, start, end])?;
            tx.execute("INSERT INTO ring_sleep (ring, start, end) VALUES (?1, ?2, ?3)", params![ring, start, end])?;
            let mut stmt =
                tx.prepare_cached("INSERT OR REPLACE INTO sleep_stages (ring, start, minutes, stage) VALUES (?1, ?2, ?3, ?4)")?;
            for (at, minutes, stage) in stages {
                stmt.execute(params![ring, at, minutes, stage])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn summary(&self, ring: &str) -> Result<Summary> {
        let info = self
            .conn
            .query_row(
                "SELECT address, name, serial, firmware, hardware, nickname, last_sync, brand FROM rings WHERE address = ?1",
                [ring],
                |r| {
                    Ok((
                        RingInfo {
                            address: r.get(0)?,
                            name: r.get(1)?,
                            serial: r.get(2)?,
                            firmware: r.get(3)?,
                            hardware: r.get(4)?,
                            nickname: r.get(5)?,
                            brand: r.get(7)?,
                        },
                        r.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .optional()?;
        let telemetry = self
            .conn
            .query_row("SELECT unix, raw FROM telemetry WHERE ring = ?1 ORDER BY unix DESC LIMIT 1", [ring], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .optional()?
            .and_then(|(unix, raw)| Descriptor::parse(&raw).map(|d| (unix, d)));
        Ok(Summary {
            last_sync: info.as_ref().and_then(|(_, s)| *s),
            ring: info.map(|(i, _)| i),
            heart_rate: self.latest_whole(ring, "heart_rate")?,
            hrv: self.latest_whole(ring, "hrv")?,
            spo2: self.latest_whole(ring, "spo2")?,
            resp_rate: self.latest(ring, "resp_rate")?,
            telemetry,
            skin_temp: self.latest_skin_temp(ring)?,
            case: self
                .conn
                .query_row(
                    "SELECT unix, case_byte FROM telemetry WHERE ring = ?1 AND case_byte != 255 ORDER BY unix DESC LIMIT 1",
                    [ring],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u8>(1)?)),
                )
                .optional()?
                .map(|(unix, byte)| (unix, byte & 0x7f, byte & 0x80 != 0)),
            spot_heart_rate: self.latest_spot(ring, MeasureKind::HeartRate)?,
            spot_spo2: self.latest_spot(ring, MeasureKind::Spo2)?,
        })
    }

    /// Replaces the previous quick reading of this kind; only the latest of each is kept.
    pub fn store_spot(&mut self, ring: &str, kind: MeasureKind, spot: &Spot) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM spot WHERE ring = ?1 AND kind = ?2", params![ring, kind.key()])?;
        tx.execute(
            "INSERT INTO spot (ring, unix, kind, value, samples, low, high) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ring, spot.unix, kind.key(), spot.value, spot.samples, spot.range.map(|r| r.0), spot.range.map(|r| r.1)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn latest_spot(&self, ring: &str, kind: MeasureKind) -> Result<Option<Spot>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value, samples, unix, low, high FROM spot WHERE ring = ?1 AND kind = ?2 ORDER BY unix DESC LIMIT 1",
                params![ring, kind.key()],
                |r| spot_from(r, 0),
            )
            .optional()?)
    }

    /// Per-minute skin temperature from history records in `[from, to)`, oldest first; the warmer
    /// of the two sensors, as for status frames. A sample counts only next to an epoch with a heart
    /// rate: off the finger the sensors read the room or the case, and the ring records no epochs.
    fn history_skin_temp(&self, ring: &str, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let span = 15 * ringconn::records::SKIN_TEMP_STEP as i64;
        let mut stmt = self.conn.prepare_cached(
            "SELECT unix FROM epochs WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND heart_rate IS NOT NULL ORDER BY unix",
        )?;
        let worn: Vec<i64> =
            stmt.query_map(params![ring, from - WORN_NEAR, to + WORN_NEAR], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let is_worn = |t: i64| {
            let i = worn.partition_point(|&e| e < t - WORN_NEAR);
            worn.get(i).is_some_and(|&e| e <= t + WORN_NEAR)
        };
        let mut stmt =
            self.conn.prepare_cached("SELECT unix, raw FROM optical WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 ORDER BY unix")?;
        let rows = stmt.query_map(params![ring, from, to + span], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut points = Vec::new();
        for row in rows {
            let (last, raw) = row?;
            let Some(samples) = ringconn::records::skin_temperatures(&raw) else { continue };
            for (i, pair) in samples.iter().enumerate() {
                let unix = last - (14 - i as i64) * ringconn::records::SKIN_TEMP_STEP as i64;
                let value = pair[0].max(pair[1]) as f64;
                if (from..to).contains(&unix) && SKIN_TEMP_RANGE.contains(&value) && is_worn(unix) {
                    points.push((unix, (value * 100.0).round() / 100.0));
                }
            }
        }
        Ok(points)
    }

    fn latest_skin_temp(&self, ring: &str) -> Result<Option<Reading<f64>>> {
        let sample: Option<Reading<f64>> = self
            .conn
            .query_row("SELECT value, unix FROM samples WHERE ring = ?1 AND kind = 'skin_temp' ORDER BY unix DESC LIMIT 1", [ring], |r| {
                Ok(Reading { value: r.get(0)?, unix: r.get(1)? })
            })
            .optional()?;
        let newest: Option<i64> = self.conn.query_row("SELECT MAX(unix) FROM optical WHERE ring = ?1", [ring], |r| r.get(0))?;
        let span = 15 * ringconn::records::SKIN_TEMP_STEP as i64;
        let record = match newest {
            Some(newest) => self.history_skin_temp(ring, newest - span, newest + 1)?.last().map(|&(unix, value)| Reading { value, unix }),
            None => None,
        };
        Ok(match (sample, record) {
            (Some(a), Some(b)) => Some(if a.unix > b.unix { a } else { b }),
            (a, b) => a.or(b),
        })
    }

    /// Points in `[from, to)`, oldest first.
    pub fn series(&self, ring: &str, metric: Metric, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let column = metric.column();
        let sql = match metric {
            // On the charger the sensor reads the case, not skin.
            Metric::SkinTemp => format!(
                "SELECT unix, {column} FROM telemetry WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND state != 4 AND {column} > 0 ORDER BY unix"
            ),
            _ => format!(
                "SELECT unix, {column} FROM epochs WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND {column} IS NOT NULL ORDER BY unix"
            ),
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![ring, from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let mut points: Vec<(i64, f64)> = rows.collect::<rusqlite::Result<_>>()?;
        if metric == Metric::SkinTemp {
            points.extend(self.history_skin_temp(ring, from, to)?);
        }
        let mut stmt =
            self.conn.prepare_cached("SELECT unix, value FROM samples WHERE ring = ?1 AND kind = ?2 AND unix >= ?3 AND unix < ?4")?;
        let rows = stmt.query_map(params![ring, metric.sample_kind(), from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let before = points.len();
        points.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        if metric == Metric::SkinTemp || points.len() > before {
            points.sort_by_key(|p| p.0);
        }
        Ok(points)
    }

    /// Worn epochs in `[from, to)` for sleep detection, oldest first.
    pub fn activity(&self, ring: &str, from: i64, to: i64) -> Result<Vec<crate::sleep::Activity>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT unix, confidence, raw, heart_rate IS NOT NULL FROM epochs
             WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND layout != 'idle' ORDER BY unix",
        )?;
        let rows = stmt.query_map(params![ring, from, to], |r| {
            let raw: Vec<u8> = r.get(2)?;
            // The five 30-second motion counts rest at 1 when the hand is still.
            let still = raw.get(10..15).is_some_and(|m| m.iter().all(|c| *c <= 1));
            Ok(crate::sleep::Activity { unix: r.get(0)?, still, pulse: r.get(3)?, confidence: r.get(1)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Estimated sleep overlapping `[from, to)`. Reads half a day either side, so a night that
    /// crosses the edge keeps its true start and end.
    pub fn sleep(&self, ring: &str, from: i64, to: i64) -> Result<Vec<(i64, i64)>> {
        if self.brand(ring)? == COLMI {
            let mut stmt =
                self.conn.prepare_cached("SELECT start, end FROM ring_sleep WHERE ring = ?1 AND end > ?2 AND start < ?3 ORDER BY start")?;
            let rows = stmt.query_map(params![ring, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
            return Ok(rows.collect::<rusqlite::Result<_>>()?);
        }
        const MARGIN: i64 = 12 * 3600;
        let epochs = self.activity(ring, from - MARGIN, to + MARGIN)?;
        Ok(crate::sleep::sessions(&epochs).into_iter().filter(|(start, end)| *end > from && *start < to).collect())
    }

    /// Seconds of restless epochs in `[start, end)`, the wakings inside a night.
    pub fn awake(&self, ring: &str, start: i64, end: i64) -> Result<i64> {
        if self.brand(ring)? == COLMI {
            let minutes: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(minutes), 0) FROM sleep_stages WHERE ring = ?1 AND start >= ?2 AND start < ?3 AND stage = ?4",
                params![ring, start, end, STAGE_AWAKE],
                |r| r.get(0),
            )?;
            return Ok(minutes * 60);
        }
        let mut stmt =
            self.conn.prepare_cached("SELECT raw FROM epochs WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND layout != 'idle'")?;
        let mut rows = stmt.query(params![ring, start, end])?;
        let mut awake = 0;
        while let Some(r) = rows.next()? {
            if crate::sleep::restless(&r.get::<_, Vec<u8>>(0)?) {
                awake += crate::sleep::EPOCH;
            }
        }
        Ok(awake)
    }

    /// Average of the nightly mean skin temperatures over the 14 days before `before`, from at
    /// least 3 nights.
    pub fn usual_skin_temp(&self, ring: &str, before: i64) -> Result<Option<f64>> {
        let nights = self.sleep(ring, before - 14 * 86_400, before)?;
        let mut means = Vec::new();
        for (start, end) in nights.into_iter().filter(|(_, end)| *end <= before) {
            if let Some(s) = Stats::of(self.series(ring, Metric::SkinTemp, start, end)?.into_iter().map(|p| p.1)) {
                means.push(s.mean);
            }
        }
        Ok((means.len() >= 3).then(|| means.iter().sum::<f64>() / means.len() as f64))
    }

    /// Data present per local day in `[from, to)`, keyed by `YYYY-MM-DD`.
    pub fn day_marks(&self, ring: &str, from: i64, to: i64) -> Result<std::collections::HashMap<String, DayMarks>> {
        let mut marks: std::collections::HashMap<String, DayMarks> = Default::default();
        let mut stmt = self.conn.prepare_cached(
            "SELECT date(unix, 'unixepoch', 'localtime') AS day, COUNT(heart_rate), COUNT(hrv), COUNT(spo2), COUNT(resp_rate)
             FROM epochs WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 GROUP BY day",
        )?;
        let rows = stmt
            .query_map(params![ring, from, to], |r| Ok((r.get::<_, String>(0)?, [r.get::<_, i64>(1)?, r.get(2)?, r.get(3)?, r.get(4)?])))?;
        for row in rows {
            let (day, counts) = row?;
            let present = Metric::EPOCH.into_iter().zip(counts).filter(|(_, c)| *c > 0).map(|(m, _)| m).collect();
            marks.entry(day).or_default().metrics = present;
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT date(unix, 'unixepoch', 'localtime') FROM telemetry WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 AND state != 4",
        )?;
        for day in stmt.query_map(params![ring, from, to], |r| r.get::<_, String>(0))? {
            marks.entry(day?).or_default().metrics.push(Metric::SkinTemp);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT date(unix, 'unixepoch', 'localtime') FROM optical WHERE ring = ?1 AND unix >= ?2 AND unix < ?3",
        )?;
        for day in stmt.query_map(params![ring, from, to], |r| r.get::<_, String>(0))? {
            let metrics = &mut marks.entry(day?).or_default().metrics;
            if !metrics.contains(&Metric::SkinTemp) {
                metrics.push(Metric::SkinTemp);
            }
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT date(unix, 'unixepoch', 'localtime'), kind FROM samples WHERE ring = ?1 AND unix >= ?2 AND unix < ?3",
        )?;
        for row in stmt.query_map(params![ring, from, to], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (day, kind) = row?;
            let Some(metric) = Metric::ALL.into_iter().find(|m| m.sample_kind() == kind) else { continue };
            let metrics = &mut marks.entry(day).or_default().metrics;
            if !metrics.contains(&metric) {
                metrics.push(metric);
            }
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT date(unix, 'unixepoch', 'localtime') FROM sport WHERE ring = ?1 AND unix >= ?2 AND unix < ?3",
        )?;
        for day in stmt.query_map(params![ring, from, to], |r| r.get::<_, String>(0))? {
            marks.entry(day?).or_default().workout = true;
        }
        Ok(marks)
    }

    /// Steps since local midnight.
    pub fn steps_today(&self, ring: &str) -> Result<(i64, StepSource)> {
        let today = chrono::Local::now().date_naive();
        self.steps(ring, local_midnight(today), local_midnight(today + chrono::Duration::days(1)))
    }

    /// Steps in `[from, to)`. The sport channel's per-interval log is used when it has records,
    /// then quarter-hour counts copied from the official app; otherwise the status frames'
    /// quarter-hour counts, which cover only the time connected.
    pub fn steps(&self, ring: &str, from: i64, to: i64) -> Result<(i64, StepSource)> {
        for table in ["sport", "steps"] {
            let (records, total): (i64, i64) = self.conn.query_row(
                &format!("SELECT COUNT(*), COALESCE(SUM(steps), 0) FROM {table} WHERE ring = ?1 AND unix >= ?2 AND unix < ?3"),
                params![ring, from, to],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if records > 0 {
                return Ok((total, StepSource::Ring));
            }
        }
        // The count restarts every quarter hour, so each quarter's highest value is its total.
        let connected: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(m), 0) FROM (SELECT MAX(steps) AS m FROM telemetry WHERE ring = ?1 AND unix >= ?2 AND unix < ?3 GROUP BY unix / 900)",
            params![ring, from, to],
            |r| r.get(0),
        )?;
        Ok((connected, StepSource::Connected))
    }

    /// Writes every import step in one transaction, so a failed import leaves nothing behind.
    pub fn import<T>(&mut self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn value(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.set_value(key, value)
    }

    /// Oldest and newest reading times.
    pub fn history_span(&self, ring: &str) -> Result<Option<(i64, i64)>> {
        let span: (Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT MIN(unix), MAX(unix) FROM (SELECT unix FROM epochs WHERE ring = ?1 UNION ALL SELECT unix FROM samples WHERE ring = ?1)",
            [ring],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(span.0.zip(span.1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringconn::records::Epoch;

    fn epoch(counter: u32, hr: u8, spo2: u8) -> Epoch {
        let mut raw = [0u8; 23];
        raw[..4].copy_from_slice(&counter.to_be_bytes());
        raw[4..10].copy_from_slice(&[hr, 50, 4, 120, spo2, 0x0a]);
        Epoch::parse(&raw).unwrap()
    }

    #[test]
    fn chosen_ring_wins_and_forgotten_rings_are_not_connected() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = ("AA:00:00:00:00:01", "BB:00:00:00:00:02");
        for r in [a, b] {
            store.upsert_ring(&RingInfo { address: r.into(), ..Default::default() }).unwrap();
        }
        store.mark_synced(a, 200).unwrap();
        store.mark_synced(b, 100).unwrap();
        assert_eq!(store.default_ring().unwrap().as_deref(), Some(a));
        store.set_default_ring(b).unwrap();
        assert_eq!(store.default_ring().unwrap().as_deref(), Some(b));
        store.forget_ring(b).unwrap();
        assert_eq!(store.default_ring().unwrap().as_deref(), Some(b));
        assert_eq!(store.ring_to_connect().unwrap(), None);
        assert!(store.rings().unwrap().iter().any(|r| r.info.address == b && r.forgotten));
        // Connecting again stores the ring anew.
        store.upsert_ring(&RingInfo { address: b.into(), ..Default::default() }).unwrap();
        assert_eq!(store.ring_to_connect().unwrap().as_deref(), Some(b));
    }

    #[test]
    fn batches_are_idempotent_and_summarised() {
        let mut store = Store::open_in_memory().unwrap();
        let ring = "F8:79:99:F8:55:BB";
        store.upsert_ring(&RingInfo { address: ring.into(), firmware: Some("FR02.018".into()), ..Default::default() }).unwrap();
        let batch = Batch::Epochs(vec![epoch(0x0ccd8601, 62, 0x12), epoch(0x0ccd8697, 61, 97)]);
        assert_eq!(store.store_batch(ring, 0, 0, &batch).unwrap(), 2);
        assert_eq!(store.store_batch(ring, 0, 0, &batch).unwrap(), 0);
        let sync = SyncRecord { started: 10, finished: 20, channel: 0, outcome: "complete".into(), pages: 1, records: 2 };
        store.record_sync(ring, &sync).unwrap();
        store.mark_synced(ring, 20).unwrap();

        let s = store.summary(ring).unwrap();
        assert_eq!(s.last_sync, Some(20));
        assert_eq!(s.heart_rate.unwrap().value, 61);
        assert_eq!(s.spo2.unwrap().value, 97);
        assert_eq!(s.resp_rate.unwrap().value, 15.0);
        assert_eq!(s.ring.unwrap().firmware.as_deref(), Some("FR02.018"));
        assert_eq!(store.default_ring().unwrap().as_deref(), Some(ring));

        let from = ringconn::time::to_unix(0x0ccd8600);
        let hr = store.series(ring, Metric::HeartRate, from, from + 3600).unwrap();
        assert_eq!(hr.iter().map(|p| p.1).collect::<Vec<_>>(), vec![62.0, 61.0]);
        assert_eq!(store.series(ring, Metric::Spo2, from, from + 3600).unwrap().len(), 1);
        let marks = store.day_marks(ring, from - 86_400, from + 86_400).unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks.values().next().unwrap().metrics, Metric::EPOCH.to_vec());
    }

    #[test]
    fn spot_measurements() {
        let mut store = Store::open_in_memory().unwrap();
        let spot = Spot { value: 62.5, samples: 14, unix: 100, range: Some((58.0, 66.0)) };
        store.store_spot("R", MeasureKind::HeartRate, &spot).unwrap();
        let s = store.summary("R").unwrap();
        assert_eq!(s.spot_heart_rate, Some(spot));
        assert_eq!(s.spot_spo2, None);
        let newer = Spot { unix: 300, ..spot };
        store.store_spot("R", MeasureKind::HeartRate, &newer).unwrap();
        let count: i64 = store.conn.query_row("SELECT COUNT(*) FROM spot", [], |r| r.get(0)).unwrap();
        assert_eq!((count, store.summary("R").unwrap().spot_heart_rate), (1, Some(newer)));
    }

    #[test]
    fn resting_rate_needs_a_full_half_hour() {
        let steady: Vec<(i64, f64)> = (0..12).map(|i| (i * 150, 60.0)).collect();
        assert_eq!(resting_heart_rate(&steady), Some(60.0));
        assert_eq!(resting_heart_rate(&steady[..8]), None);
        let mut day = steady.clone();
        day.extend((12..40).map(|i| (i * 150, 80.0)));
        assert_eq!(resting_heart_rate(&day), Some(60.0));
    }

    #[test]
    fn settings_rules_and_case_battery() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.settings().unwrap(), Settings::default());
        let changed = Settings { stay_connected: false, sync_interval_minutes: Some(60), ..Default::default() };
        store.save_settings(&changed).unwrap();
        assert_eq!(store.settings().unwrap(), changed);

        let defaults = store.rules().unwrap();
        assert_eq!(
            defaults.iter().map(|r| (r.stat, r.op, r.threshold)).collect::<Vec<_>>(),
            vec![(Stat::Battery, Op::Lt, 5.0), (Stat::DaysSinceSync, Op::Gt, 5.0)]
        );
        let id = store.add_rule(Stat::Spo2, Op::Lt, 90.0).unwrap();
        store.update_rule(&Rule { id, stat: Stat::Spo2, op: Op::Le, threshold: 88.0 }).unwrap();
        store.delete_rule(defaults[0].id).unwrap();
        let rules = store.rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1], Rule { id, stat: Stat::Spo2, op: Op::Le, threshold: 88.0 });
    }

    #[test]
    fn steps_sum_quarter_hours() {
        let store = Store::open_in_memory().unwrap();
        let frame = |steps: u16| {
            let mut f = vec![0x87, 70, 0x02, 0, 0, 0, 0x01, 0x50, 0x01, 0x4c, 0, 0, 0, 0, 0x10, 0x00, 0x5b, 0xff];
            f[4..6].copy_from_slice(&steps.to_be_bytes());
            f.push(ringconn::frame::xor(&f));
            f
        };
        for (unix, steps) in [(900, 10), (1000, 40), (1700, 55), (1800, 3), (2000, 20)] {
            let f = frame(steps);
            store.store_telemetry("R", unix, &Descriptor::parse(&f).unwrap(), &f).unwrap();
        }
        assert_eq!(store.steps("R", 0, 3600).unwrap(), (75, StepSource::Connected));
        assert_eq!(store.steps("R", 1800, 3600).unwrap(), (20, StepSource::Connected));
    }

    #[test]
    fn skin_temperature_skips_the_charger() {
        let store = Store::open_in_memory().unwrap();
        for (unix, state) in [(100, 0x02u8), (200, 0x04)] {
            let mut f = vec![0x87, 70, state, 0, 0, 0, 0x01, 0x50, 0x01, 0x4c, 0, 0, 0, 0, 0x10, 0x00, 0x5b, 0xff];
            f.push(ringconn::frame::xor(&f));
            store.store_telemetry("R", unix, &Descriptor::parse(&f).unwrap(), &f).unwrap();
        }
        let points = store.series("R", Metric::SkinTemp, 0, 300).unwrap();
        assert_eq!(points.len(), 1);
        assert!((points[0].1 - 33.6).abs() < 1e-4, "{points:?}");
        let marks = store.day_marks("R", 0, 300).unwrap();
        assert_eq!(marks.values().next().unwrap().metrics, vec![Metric::SkinTemp]);
    }

    #[test]
    fn last_case_reading() {
        let store = Store::open_in_memory().unwrap();
        for (unix, case) in [(100, 0xc0u8), (200, 0x3c), (300, 0xff)] {
            let mut f = vec![0x87, 70, 0x02, 0, 0, 0, 0x01, 0x50, 0x01, 0x4c, 0, 0, 0, 0, 0x10, 0x00, 0x5b, case];
            f.push(ringconn::frame::xor(&f));
            store.store_telemetry("R", unix, &Descriptor::parse(&f).unwrap(), &f).unwrap();
        }
        assert_eq!(store.summary("R").unwrap().case, Some((200, 60, false)));
    }

    #[test]
    fn colmi_rings_read_samples_and_ring_sleep() {
        let mut store = Store::open_in_memory().unwrap();
        let ring = "AA:BB:CC:DD:EE:01";
        let info = RingInfo { address: ring.into(), name: Some("R02_0101".into()), brand: Some(COLMI.into()), ..Default::default() };
        store.upsert_ring(&info).unwrap();
        // A later connect that does not know the brand keeps it.
        store.upsert_ring(&RingInfo { address: ring.into(), ..Default::default() }).unwrap();
        assert_eq!(store.brand(ring).unwrap(), COLMI);
        assert_eq!(store.brand("unknown").unwrap(), RINGCONN);

        let hr = Metric::HeartRate.sample_kind();
        assert_eq!(store.store_samples(ring, hr, &[(1000, 60.0), (1300, 64.0)]).unwrap(), 2);
        store.store_samples(ring, hr, &[(1300, 66.0)]).unwrap();
        assert_eq!(store.series(ring, Metric::HeartRate, 0, 2000).unwrap(), vec![(1000, 60.0), (1300, 66.0)]);
        assert_eq!(store.history_span(ring).unwrap(), Some((1000, 1300)));
        store.store_samples(ring, Metric::Spo2.sample_kind(), &[(1200, 96.5)]).unwrap();
        let summary = store.summary(ring).unwrap();
        assert_eq!((summary.heart_rate.unwrap().value, summary.spo2.unwrap().value), (66, 97));

        let night = (10_000, 10_000 + 8 * 3600, vec![(10_000, 120, 2), (17_200, 10, 5), (17_800, 350, 3)]);
        store.store_ring_sleep(ring, &[night]).unwrap();
        // A rescored night that overlaps replaces the first.
        let rescored = (10_600, 10_000 + 8 * 3600, vec![(10_600, 100, 2), (16_600, 20, 5)]);
        store.store_ring_sleep(ring, &[rescored]).unwrap();
        assert_eq!(store.sleep(ring, 0, 100_000).unwrap(), vec![(10_600, 38_800)]);
        assert_eq!(store.awake(ring, 10_600, 38_800).unwrap(), 20 * 60);
    }

    #[test]
    fn nickname_survives_reconnects() {
        let store = Store::open_in_memory().unwrap();
        let ring = "F8:79:99:F8:55:BB";
        let info = RingInfo { address: ring.into(), name: Some("RingConn Gen2-55BB".into()), ..Default::default() };
        store.upsert_ring(&info).unwrap();
        store.set_nickname(ring, " Left hand ").unwrap();
        store.upsert_ring(&info).unwrap();
        assert_eq!(store.ring(ring).unwrap().unwrap().display_name(), "Left hand");
        store.set_nickname(ring, "  ").unwrap();
        assert_eq!(store.ring(ring).unwrap().unwrap().display_name(), "RingConn Gen2-55BB");
    }

    #[test]
    fn wakings_inside_a_night() {
        let mut store = Store::open_in_memory().unwrap();
        let base = 0x0ccd_8600u32;
        let mut epochs: Vec<Epoch> = (0..8).map(|i| epoch(base + i * 150, 60, 95)).collect();
        epochs[3].raw[9] = 0x0b;
        epochs[5].raw[9] = 0x0c;
        store.store_batch("R", 0, 0, &Batch::Epochs(epochs)).unwrap();
        let start = ringconn::time::to_unix(base);
        assert_eq!(store.awake("R", start, start + 8 * 150).unwrap(), 300);
        assert_eq!(store.usual_skin_temp("R", start + 8 * 150).unwrap(), None);
    }

    #[test]
    fn hour_rules_become_day_rules() {
        let conn = Connection::open_in_memory().unwrap();
        for m in &MIGRATIONS[..6] {
            conn.execute_batch(m).unwrap();
        }
        conn.execute("INSERT INTO notification_rules (stat, op, threshold) VALUES ('hours_since_sync', '>', 72)", []).unwrap();
        conn.pragma_update(None, "user_version", 6).unwrap();
        let store = Store::init(conn).unwrap();
        let days: Vec<f64> = store.rules().unwrap().iter().filter(|r| r.stat == Stat::DaysSinceSync).map(|r| r.threshold).collect();
        assert_eq!(days, vec![5.0, 3.0]);
    }

    #[test]
    fn skin_temperature_from_history() {
        let store = Store::open_in_memory().unwrap();
        let raw = ringconn::frame::parse_hex(
            "0c c4 2d b8 02 a5 05 18 c3 80 a0 18 0a 00 80 a0 08 06 00 95 65 29 5e 57 \
             96 65 79 6e 5a 97 25 c9 72 5b 97 65 c9 6e 5a 95 e5 59 52 51 94 64 d0",
        )
        .unwrap();
        let counter = u32::from_be_bytes(raw[..4].try_into().unwrap());
        let last = ringconn::time::to_unix(counter);
        store.conn.execute("INSERT INTO optical (ring, counter, unix, raw) VALUES ('R', ?1, ?2, ?3)", params![counter, last, raw]).unwrap();
        assert!(store.series("R", Metric::SkinTemp, last - 3600, last + 1).unwrap().is_empty(), "no epochs: not worn");
        // Worn epochs 2.5 minutes apart across the record.
        let mut store = store;
        let worn: Vec<Epoch> = (0..7).map(|i| epoch(counter - 900 + i * 150, 60, 0x12)).collect();
        store.store_batch("R", 0, 0, &Batch::Epochs(worn)).unwrap();
        let points = store.series("R", Metric::SkinTemp, last - 3600, last + 1).unwrap();
        assert_eq!(points.len(), 15);
        assert_eq!(points[0], (last - 14 * 60, 25.7));
        assert_eq!(points[14].0, last);
        assert_eq!(store.summary("R").unwrap().skin_temp.map(|r| r.unix), Some(last));
        let marks = store.day_marks("R", last - 86_400, last + 86_400).unwrap();
        assert!(marks.values().any(|m| m.metrics.contains(&Metric::SkinTemp)));
    }

    #[test]
    fn migrates_a_version_one_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let mut store = Store::init(conn).unwrap();
        store.store_spot("R", MeasureKind::Spo2, &Spot { value: 97.0, samples: 9, unix: 1, range: None }).unwrap();
    }
}
