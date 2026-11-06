//! Imports history from a ringprobe report (every intact page in its frame log, plus the ring's
//! identity), and from the official Android app's database (`ring_conn.db`).

use crate::store::{insert_epoch, RingInfo, Store, IMPORTED_CHANNEL};
use anyhow::{bail, Context, Result};
use ringconn::drain::Batch;
use ringconn::frame::{checksum_ok, op};
use ringconn::records::{Epoch, OpticalRecord, Page, SportRecord, EPOCH_LEN};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;

/// Settings key holding the time of the last import from the official app.
pub const OFFICIAL_IMPORT_KEY: &str = "official_import";

#[derive(Debug, Default)]
pub struct OfficialSummary {
    pub epochs: usize,
    pub new_epochs: usize,
    pub steps: usize,
    pub records: usize,
    pub first: Option<i64>,
    pub last: Option<i64>,
}

/// The official app's own results and summaries, copied as JSON rows into `official`. Account,
/// chat, weather and traffic tables are left out. The time column orders the rows.
const OFFICIAL_TABLES: &[(&str, &str)] = &[
    ("SleepSyncModel", "bedtimeStart"),
    ("SleepSummaryModel", "utc"),
    ("SleepNapModel", "start"),
    ("OSADataModel", "startTime"),
    ("OSAReportModel", "startUtc"),
    ("OSAMonthlyModel", "utc"),
    ("TempOffsetModel", "utc"),
    ("DailyModel", "utc"),
    ("WeeklyModel", "createTimeStamp"),
    ("YearlyModel", "utc"),
    ("ActivityModel", "utc"),
    ("CalorieSyncModel", "utc"),
    ("StandSyncModel", "utc"),
    ("PressureAsyncModel", "utc"),
    ("PressureBalanceModel", "utc"),
    ("ActivityBalanceModel", "utc"),
    ("BasalVitalBalanceModelV2", "utc"),
    ("PhysicalMentalBalanceModelV2", "utc"),
    ("AlertHeadacheModelTable", "start"),
    ("AlertVitalModelTable", "start"),
    ("SportRecordModel", "startTime"),
    ("LifestyleInfoTable", "utc"),
    ("MenstrualCycleModel", "utc"),
];

/// Columns that identify the account or phone rather than describe the data.
const SKIPPED_COLUMNS: &[&str] = &["uid", "userId", "deviceId"];

/// A JSON list of numbers as the app stores them, e.g. `[1,1,15,1,1]`.
fn numbers(text: Option<&str>) -> Vec<u32> {
    text.unwrap_or("").trim().trim_start_matches('[').trim_end_matches(']').split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

/// Rebuilds the ring's 23-byte `0x4c` record from the app's columns (PROTOCOL.md section 10.1).
/// The app keeps the ring's own bytes beside its cleaned values: `rawHr`, `rawHrv`, and `rawSpo2`,
/// which also carries the activity layout tag.
#[allow(clippy::too_many_arguments)]
fn epoch_record(
    unix: i64,
    heart_rate: u32,
    hrv: u32,
    confidence: u32,
    resp_x8: u32,
    byte8: Option<u32>,
    marker: Option<u32>,
    motion: &[u32],
    magnitudes: &[u32],
) -> Option<Epoch> {
    let counter = u32::try_from(unix - ringconn::time::EPOCH).ok()?;
    let byte = |v: u32| v.min(255) as u8;
    let mut raw = [0u8; EPOCH_LEN];
    raw[..4].copy_from_slice(&counter.to_be_bytes());
    raw[4] = byte(heart_rate);
    raw[5] = byte(hrv);
    raw[6] = byte(confidence);
    raw[7] = byte(resp_x8);
    raw[8] = byte8.map_or(0x12, byte);
    raw[9] = marker.map_or(0x0a, byte);
    for (i, slot) in raw[10..15].iter_mut().enumerate() {
        *slot = motion.get(i).map_or(1, |&m| byte(m));
    }
    // Five 12-bit values, most significant first, then a 4-bit flag left at zero.
    let mut packed: u64 = 0;
    for i in 0..5 {
        packed = (packed << 12) | magnitudes.get(i).map_or(0, |&m| m.min(0xfff)) as u64;
    }
    raw[15..23].copy_from_slice(&(packed << 4).to_be_bytes());
    Epoch::parse(&raw)
}

/// Copies `path` and its write-ahead log beside a private copy, so the original is never opened
/// for writing and SQLite can create its side files.
fn private_copy(path: &Path) -> Result<tempdir::Dir> {
    let dir = tempdir::Dir::new()?;
    std::fs::copy(path, dir.db()).with_context(|| format!("cannot read {}", path.display()))?;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::copy(&wal, dir.path.join("ring_conn.db-wal"));
    Ok(dir)
}

mod tempdir {
    pub struct Dir {
        pub path: std::path::PathBuf,
    }

    impl Dir {
        pub fn new() -> anyhow::Result<Self> {
            let path = std::env::temp_dir().join(format!("ringmaster-import-{}", std::process::id()));
            std::fs::create_dir_all(&path)?;
            Ok(Self { path })
        }

        pub fn db(&self) -> std::path::PathBuf {
            self.path.join("ring_conn.db")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Copies all history from the official app's `ring_conn.db` into `ring`. Rows already present
/// are skipped, so running it again adds only what is new.
pub fn official_app(store: &mut Store, ring: &str, path: &Path) -> Result<OfficialSummary> {
    let copy = private_copy(path)?;
    let app = Connection::open_with_flags(copy.db(), OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("cannot open the copy as an SQLite database")?;
    let has = |table: &str| -> Result<bool> {
        Ok(app.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |_| Ok(())).optional()?.is_some())
    };
    if !has("HistoryHrSyncInfo")? {
        bail!("this is not the RingConn app's database (ring_conn.db)");
    }

    let mut summary = OfficialSummary::default();
    store.import(|tx| {
        let mut stmt = app.prepare(
            "SELECT h.utcTs, COALESCE(h.rawHr, h.pr, 0), COALESCE(h.rawHrv, h.hrv, 0), COALESCE(h.confidence, 0),
                    COALESCE(h.resprate, 0), s.rawSpo2, h.item2p5, h.envLights, h.actiCountsString
             FROM HistoryHrSyncInfo h LEFT JOIN HistorySpo2SyncInfo s ON s.utcTs = h.utcTs AND s.deviceId = h.deviceId
             GROUP BY h.utcTs ORDER BY h.utcTs",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let unix: i64 = r.get(0)?;
            let (motion, magnitudes) =
                (numbers(r.get::<_, Option<String>>(7)?.as_deref()), numbers(r.get::<_, Option<String>>(8)?.as_deref()));
            let Some(e) = epoch_record(unix, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, &motion, &magnitudes) else {
                continue;
            };
            summary.epochs += 1;
            summary.new_epochs += insert_epoch(tx, ring, IMPORTED_CHANNEL, &e)?;
            summary.first = Some(summary.first.map_or(unix, |f| f.min(unix)));
            summary.last = Some(summary.last.map_or(unix, |l| l.max(unix)));
        }

        if has("StepsInfo")? {
            let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO steps (ring, unix, steps, distance) VALUES (?1, ?2, ?3, ?4)")?;
            let mut stmt = app.prepare("SELECT utcTs, steps, distance FROM StepsInfo WHERE steps > 0")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                summary.steps += insert.execute(params![ring, r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?])?;
            }
        }

        let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO official (ring, kind, unix, data) VALUES (?1, ?2, ?3, ?4)")?;
        for (table, time) in OFFICIAL_TABLES {
            if !has(table)? {
                continue;
            }
            let columns: Vec<String> = app
                .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<_>>()?;
            let fields: Vec<String> =
                columns.iter().filter(|c| !SKIPPED_COLUMNS.contains(&c.as_str())).map(|c| format!("'{c}', \"{c}\"")).collect();
            // Some tables count milliseconds.
            let sql = format!(
                "SELECT CASE WHEN \"{time}\" > 100000000000 THEN \"{time}\" / 1000 ELSE \"{time}\" END, json_object({}) FROM \"{table}\"",
                fields.join(", ")
            );
            let mut stmt = app.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                summary.records += insert.execute(params![ring, table, r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?])?;
            }
        }
        Ok(())
    })?;
    store.set_setting(OFFICIAL_IMPORT_KEY, &chrono::Utc::now().timestamp().to_string())?;
    Ok(summary)
}

#[derive(Debug, Default)]
pub struct ImportSummary {
    pub ring: String,
    pub pages: usize,
    pub skipped: usize,
    pub new_rows: usize,
}

fn field(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| l.trim_start().strip_prefix(name)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && v != "?")
}

pub fn probe_report(store: &mut Store, path: &Path) -> Result<ImportSummary> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let Some(ring) = text.lines().find_map(|l| l.split("--  using ").nth(1)).map(|a| a.trim().to_string()) else {
        bail!("not a ringprobe report: no ring address found");
    };
    store.upsert_ring(&RingInfo {
        address: ring.clone(),
        name: field(&text, "name           "),
        serial: field(&text, "serial         "),
        firmware: field(&text, "firmware       "),
        hardware: field(&text, "hardware       "),
        nickname: None,
        brand: Some(crate::store::RINGCONN.into()),
    })?;

    let mut summary = ImportSummary { ring: ring.clone(), ..Default::default() };
    let mut channel = 0u8;
    for line in text.lines() {
        if let Some(rest) = line.split("--  channel ").nth(1) {
            if rest.contains("sync open") {
                channel = u8::from_str_radix(&rest[..2], 16).unwrap_or(0);
            }
            continue;
        }
        let Some(rx) = line.split("] RX  ").nth(1) else { continue };
        // Labels after the hex never contain a two-character hex word.
        let frame: Vec<u8> = rx.split_whitespace().filter(|t| t.len() == 2).map_while(|t| u8::from_str_radix(t, 16).ok()).collect();
        let Some(page) = Page::parse(&frame) else { continue };
        if checksum_ok(&frame) != Some(true) {
            summary.skipped += 1;
            continue;
        }
        let batch = match page.op {
            op::EPOCH_PAGE => Batch::Epochs(page.records.iter().filter_map(|r| Epoch::parse(r)).collect()),
            op::SPORT_PAGE => Batch::Sport(page.records.iter().filter_map(|r| SportRecord::parse(r)).collect()),
            _ => Batch::Optical(page.records.iter().filter_map(|r| OpticalRecord::parse(r)).collect()),
        };
        summary.new_rows += store.store_batch(&ring, channel, 0, &batch)?;
        summary.pages += 1;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cut-down `ring_conn.db` with the columns the import reads.
    fn official_db(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE HistoryHrSyncInfo (deviceId INTEGER, utcTs INTEGER, pr INTEGER, hrv INTEGER, resprate INTEGER, item2p5 INTEGER,
                 envLights TEXT, confidence INTEGER, actiCountsString TEXT, rawHr INTEGER, rawHrv INTEGER, uid INTEGER);
             CREATE TABLE HistorySpo2SyncInfo (deviceId INTEGER, utcTs INTEGER, spo2 INTEGER, rawSpo2 INTEGER, uid INTEGER);
             CREATE TABLE StepsInfo (deviceId INTEGER, utcTs INTEGER, steps INTEGER, distance REAL, uid INTEGER);
             CREATE TABLE SleepSyncModel (id INTEGER, uid INTEGER, bedtimeStart INTEGER, sleepPhases TEXT);
             CREATE TABLE UserInfo (id INTEGER, name TEXT, phone TEXT);
             INSERT INTO HistoryHrSyncInfo VALUES
                 (7, 1780459559, 56, 35, 124, 10, '[1,1,1,1,1]', 10, '[0,0,23,0,0]', 56, 35, 9),
                 (7, 1780459709, 0, 0, 0, 13, '[15,1,1,1,1]', 0, '[2188,2079,422,87,90]', 4, 0, 9);
             INSERT INTO HistorySpo2SyncInfo VALUES (7, 1780459559, 96, 94, 9), (7, 1780459709, 0, 18, 9);
             INSERT INTO StepsInfo VALUES (7, 1780459200, 40, 30.5, 9), (7, 1780460100, 0, 0, 9);
             INSERT INTO SleepSyncModel VALUES (1, 9, 1780459559, '[{\"phase\":4}]');
             INSERT INTO UserInfo VALUES (1, 'Someone', '555');",
        )
        .unwrap();
    }

    #[test]
    fn official_app_history() {
        let dir = std::env::temp_dir().join(format!("ringmaster-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ring_conn.db");
        official_db(&path);
        let mut store = Store::open_in_memory().unwrap();

        let s = official_app(&mut store, "R", &path).unwrap();
        assert_eq!((s.epochs, s.new_epochs, s.steps, s.records), (2, 2, 1, 1));
        assert_eq!((s.first, s.last), (Some(1780459559), Some(1780459709)));
        let again = official_app(&mut store, "R", &path).unwrap();
        assert_eq!((again.new_epochs, again.steps, again.records), (0, 0, 0), "a second run adds nothing");
        assert!(store.value(OFFICIAL_IMPORT_KEY).unwrap().is_some());
        assert!(path.exists(), "the original is left in place");

        // The first record keeps the ring's raw SpO2 (94), not the app's corrected 96.
        let summary = store.summary("R").unwrap();
        assert_eq!(summary.spo2.map(|r| (r.value, r.unix)), Some((94, 1780459559)));
        assert_eq!(summary.heart_rate.map(|r| r.value), Some(56));
        let hr = store.series("R", crate::store::Metric::HeartRate, 1780459000, 1780460000).unwrap();
        assert_eq!(hr.len(), 1, "the sentinel 4 is not a heart rate");
        let activity = store.activity("R", 1780459000, 1780460000).unwrap();
        assert_eq!(activity.iter().map(|a| a.still).collect::<Vec<_>>(), vec![true, false]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn magnitudes_pack_most_significant_first() {
        let e =
            epoch_record(ringconn::time::EPOCH + 150, 60, 30, 10, 120, Some(97), Some(10), &[1; 5], &[2188, 2079, 422, 87, 90]).unwrap();
        let v = u64::from_be_bytes(e.raw[15..23].try_into().unwrap());
        let unpacked: Vec<u64> = (0..5).map(|i| (v >> (64 - 12 * (i + 1))) & 0xfff).collect();
        assert_eq!(unpacked, vec![2188, 2079, 422, 87, 90]);
        assert_eq!((e.counter, e.spo2(), e.heart_rate()), (150, Some(97), Some(60)));
        assert_eq!(epoch_record(ringconn::time::EPOCH - 1, 60, 0, 0, 0, None, None, &[], &[]), None);
    }
}
