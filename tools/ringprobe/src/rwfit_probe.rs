//! `--rwfit`: a first look at an RWfit ring (RWFit app family). Finds the ring by its
//! advertisement, lists its services, works out the framing, then on the legacy framing reads
//! identity, binding, features, battery and every history stream, acknowledging each frame as the
//! protocol requires. Records are decoded into the report; nothing is stored elsewhere.

use crate::report::Report;
use crate::transport;
use crate::Shared;
use anyhow::{anyhow, bail, Context, Result};
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, Device};
use chrono::{Datelike, Local, Timelike};
use futures::{pin_mut, StreamExt};
use rwfit::legacy::{self, cmd, Inbound};
use rwfit::{jieli, records, Framing};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

/// The bind id the ring stores and echoes.
const USER_ID: &str = "RM";
/// Standard Heart Rate service and its measurement characteristic.
const HEART_RATE_SERVICE: uuid::Uuid = uuid::Uuid::from_u128(0x0000180d_0000_1000_8000_00805f9b34fb);
const HEART_RATE_MEASUREMENT: uuid::Uuid = uuid::Uuid::from_u128(0x00002a37_0000_1000_8000_00805f9b34fb);
/// A legacy history stream ends with this long a silence after its last frame.
const SETTLE: Duration = Duration::from_millis(1500);

fn local_time(local: u32) -> String {
    chrono::DateTime::from_timestamp(local as i64, 0).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| local.to_string())
}

fn label(frame: &[u8]) -> String {
    match frame.first() {
        Some(0x7e) if frame.len() >= 8 => {
            let c = frame[2];
            let bad =
                if frame.len() >= 8 + frame[4] as usize && legacy::xor(&frame[8..8 + frame[4] as usize]) != frame[7] && frame[3] & 8 == 0 {
                    "  BAD XOR"
                } else {
                    ""
                };
            format!("legacy {}{}{bad}", legacy::name(c), if frame[3] & 8 != 0 { " (chunk)" } else { "" })
        }
        Some(0xab) => "JieLi".into(),
        _ => "?".into(),
    }
}

/// Scans even when the address is known: BlueZ forgets an unbonded device shortly after it was
/// last seen, and connecting then fails with "the target object was either not present or
/// removed" (2026-11-04).
async fn find(adapter: &Adapter, report: &Report, want: Option<Address>) -> Result<Device> {
    report.note(match want {
        Some(a) => format!("scanning up to 30 s for {a}"),
        None => "scanning up to 30 s for an RWfit ring (service A00A or company 0x05D6/0x06D6)".into(),
    });
    let stream = adapter.discover_devices().await.context("discovery would not start")?;
    pin_mut!(stream);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen: Vec<Device> = Vec::new();
    loop {
        for d in &seen {
            let uuids = d.uuids().await.ok().flatten().unwrap_or_default();
            let mfg = d.manufacturer_data().await.ok().flatten().unwrap_or_default();
            let matched = match want {
                Some(a) => d.address() == a && d.rssi().await.ok().flatten().is_some(),
                None => rwfit::is_rwfit(uuids.iter(), mfg.iter().map(|(k, v)| (*k, v.as_slice()))),
            };
            if matched {
                let name = d.name().await.ok().flatten().unwrap_or_default();
                let rssi = d.rssi().await.ok().flatten();
                let mut lines = vec![format!("address        {}", d.address()), format!("name           {name}")];
                if let Some(r) = rssi {
                    lines.push(format!("rssi           {r} dBm"));
                }
                for (k, v) in &mfg {
                    lines.push(format!("manufacturer   {k:#06x}: {}", rwfit::hex(v)));
                }
                for u in &uuids {
                    lines.push(format!("service        {u}"));
                }
                report.section("Advertisement", lines);
                report.note(format!("found {} \"{name}\"", d.address()));
                return Ok(d.clone());
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("no RWfit ring advertising; keep it close to the phone and run again");
        }
        if let Ok(Some(AdapterEvent::DeviceAdded(a))) = timeout(left.min(Duration::from_millis(500)), stream.next()).await {
            if let Ok(d) = adapter.device(a) {
                seen.push(d);
            }
        }
    }
}

/// The write/notify pair on A00A, with every notification logged and passed on.
struct Link {
    write: Characteristic,
    request: bool,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    report: Arc<Report>,
    serial: u16,
    decoder: legacy::Decoder,
    reader: tokio::task::AbortHandle,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Link {
    async fn open(device: &Device, report: Arc<Report>) -> Result<Self> {
        let (mut write, mut notify) = (None, None);
        for s in device.services().await? {
            if s.uuid().await? != rwfit::SERVICE {
                continue;
            }
            for c in s.characteristics().await? {
                match c.uuid().await? {
                    u if u == rwfit::WRITE => write = Some(c),
                    u if u == rwfit::NOTIFY => notify = Some(c),
                    _ => {}
                }
            }
        }
        let write = write.context("write characteristic B002 not found")?;
        let notify = notify.context("notify characteristic B003 not found")?;
        let request = write.flags().await.map(|f| f.write).unwrap_or(true);
        let stream = notify.notify().await.context("enabling notifications on B003 failed")?;
        report.note(format!("notifications enabled on B003; writes {}", if request { "with response" } else { "without response" }));
        let (tx, rx) = mpsc::unbounded_channel();
        let r = report.clone();
        let reader = tokio::spawn(async move {
            pin_mut!(stream);
            while let Some(f) = stream.next().await {
                r.rx(&f, &label(&f));
                if tx.send(f).is_err() {
                    break;
                }
            }
            r.note("notification stream ended");
        });
        Ok(Self { write, request, rx, report, serial: 0, decoder: legacy::Decoder::default(), reader: reader.abort_handle() })
    }

    async fn raw(&mut self, bytes: &[u8], what: &str) -> Result<()> {
        self.report.tx(bytes, what);
        let req =
            CharacteristicWriteRequest { op_type: if self.request { WriteOp::Request } else { WriteOp::Command }, ..Default::default() };
        self.write.write_ext(bytes, &req).await.with_context(|| format!("write {} failed", rwfit::hex(bytes)))
    }

    fn next_serial(&mut self) -> u16 {
        self.serial = if self.serial == u16::MAX { 1 } else { self.serial + 1 };
        self.serial
    }

    async fn send(&mut self, c: u8, payload: &[u8]) -> Result<()> {
        let s = self.next_serial();
        self.raw(&legacy::encode(c, payload, s), &format!("legacy {}", legacy::name(c))).await
    }

    /// Legacy frames until `quiet` passes without one or `max` elapses, acknowledging each.
    /// Returns whole frames and whether the ring acknowledged command `ack_of`.
    async fn legacy_frames(&mut self, ack_of: u8, quiet: Duration, max: Duration) -> Result<(Vec<(u8, Vec<u8>)>, bool)> {
        let deadline = Instant::now() + max;
        let (mut frames, mut acked) = (Vec::new(), false);
        loop {
            let left = deadline.saturating_duration_since(Instant::now()).min(quiet);
            if left.is_zero() {
                return Ok((frames, acked));
            }
            let Ok(Some(n)) = timeout(left, self.rx.recv()).await else { return Ok((frames, acked)) };
            for item in self.decoder.decode(&n) {
                match item {
                    Inbound::AckNeeded { cmd: c, serial } => {
                        let s = self.next_serial();
                        self.raw(&legacy::ack(c, serial, legacy::ACK_OK, s), "app ack").await?;
                    }
                    Inbound::ChecksumFailed { cmd: c, serial } => {
                        let s = self.next_serial();
                        self.raw(&legacy::ack(c, serial, legacy::ACK_BAD_CHECKSUM, s), "app ack: bad XOR, resend").await?;
                    }
                    Inbound::DeviceAck { cmd: c, status, .. } => {
                        if c == ack_of {
                            acked = true;
                        }
                        if status != 0 {
                            self.report.note(format!("ring acknowledged {} with status {status}", legacy::name(c)));
                        }
                    }
                    Inbound::Frame { cmd: c, payload } => frames.push((c, payload)),
                }
            }
        }
    }

    /// One request and its reply frame.
    async fn request(&mut self, c: u8, payload: &[u8]) -> Result<(Option<Vec<u8>>, bool)> {
        self.send(c, payload).await?;
        let (frames, acked) = self.legacy_frames(c, Duration::from_secs(3), Duration::from_secs(5)).await?;
        Ok((frames.into_iter().find(|(fc, _)| *fc == c).map(|(_, p)| p), acked))
    }

    async fn jieli(&mut self, triple: [u8; 3]) -> Result<Option<Vec<u8>>> {
        self.raw(&jieli::encode(&triple, jieli::REQUEST), "JieLi request").await?;
        let mut d = jieli::Decoder::default();
        let deadline = Instant::now() + Duration::from_secs(4);
        while let Ok(Some(n)) = timeout(deadline.saturating_duration_since(Instant::now()), self.rx.recv()).await {
            for item in d.decode(&n) {
                if let jieli::Inbound::Frame { flag, payload } = item {
                    if flag == jieli::REQUEST {
                        let t = [payload[0], payload[1], payload[2]];
                        self.raw(&jieli::ack(t), "JieLi ack").await?;
                    }
                    if payload.starts_with(&triple[..2]) {
                        return Ok(Some(payload));
                    }
                }
            }
        }
        Ok(None)
    }
}

pub async fn run(want: Option<Address>, report: Arc<Report>, shared: Arc<Shared>) -> Result<()> {
    let session = bluer::Session::new().await.context("cannot reach BlueZ over D-Bus")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.context("cannot power the adapter")?;
    let device = find(&adapter, &report, want).await?;
    *shared.device.lock().unwrap() = Some(device.clone());
    transport::connect(&device, &report).await?;
    if !transport::wait_ready(&device, &report, Duration::from_secs(30)).await {
        bail!("services not readable after 30 s");
    }
    if let Ok((lines, _)) = transport::gatt_table(&device).await {
        report.section("GATT layout", lines);
    }
    let services: Vec<uuid::Uuid> = {
        let mut v = Vec::new();
        for s in device.services().await? {
            v.push(s.uuid().await?);
        }
        v
    };
    let hint = Framing::from_services(services.iter());
    report.note(format!("framing suggested by the services: {hint:?}"));

    let mut link = Link::open(&device, report.clone()).await?;
    let mut found = Vec::new();

    let (info, acked) = link.request(cmd::DEVICE_INFO, &[]).await?;
    let framing = if info.is_some() || acked {
        Framing::Legacy
    } else if link.jieli(jieli::DEVICE_INFO).await?.is_some() {
        Framing::JieLi
    } else {
        bail!("no answer to device info in either framing");
    };
    found.push(format!("framing        {framing:?} (services suggested {hint:?})"));
    if framing == Framing::JieLi {
        let battery = link.jieli(jieli::BATTERY).await?;
        let menu = link.jieli(jieli::FUNCTION_MENU).await?;
        found.push(format!("battery reply  {}", battery.map(|b| rwfit::hex(&b)).unwrap_or_else(|| "none".into())));
        found.push(format!("function menu  {}", menu.map(|b| rwfit::hex(&b)).unwrap_or_else(|| "none".into())));
        found.push("this probe reads history only on the legacy framing; send this report".into());
        report.finding("RWfit ring", found.join("\n    "));
        return Ok(());
    }
    if let Some((class, number)) = info.as_deref().and_then(records::device_info) {
        found.push(format!("device         class \"{class}\", number \"{number}\""));
    }

    let (bind, _) = link.request(cmd::BIND_STATUS, &[]).await?;
    let bound = bind.as_deref().and_then(records::bind_status);
    found.push(format!("bind status    {}", bind.as_deref().map(rwfit::hex).unwrap_or_else(|| "no reply".into())));
    if bound.is_some_and(|(b, _)| !b) {
        report.note(format!("ring not bound; binding with id \"{USER_ID}\" as the RWFit app does before reading history"));
        let (_, acked) = link.request(cmd::BIND, &legacy::bind(USER_ID)).await?;
        let (after, _) = link.request(cmd::BIND_STATUS, &[]).await?;
        found.push(format!(
            "bind           {}; status now {}",
            if acked { "acknowledged" } else { "not acknowledged" },
            after.as_deref().map(rwfit::hex).unwrap_or_else(|| "no reply".into())
        ));
    }

    let now = Local::now();
    let (_, acked) = link
        .request(
            cmd::SET_TIME,
            &legacy::set_time(
                now.year() as u16,
                now.month() as u8,
                now.day() as u8,
                now.hour() as u8,
                now.minute() as u8,
                now.second() as u8,
            ),
        )
        .await?;
    found.push(format!("set time       {} ({})", now.format("%H:%M:%S local"), if acked { "acknowledged" } else { "no acknowledgement" }));

    let (feat, _) = link.request(cmd::FEATURES, &[]).await?;
    let features = feat.as_deref().and_then(|f| f.first().copied()).map(records::Features);
    found.push(format!(
        "features       {} = {}",
        feat.as_deref().map(rwfit::hex).unwrap_or_else(|| "no reply".into()),
        features.map(|f| f.names().join(", ")).unwrap_or_default()
    ));
    let (bat, _) = link.request(cmd::BATTERY, &[]).await?;
    found.push(format!(
        "battery        {}",
        bat.as_deref()
            .and_then(records::battery)
            .map(|b| format!("{} % (status {}, low {})", b.percent, b.status, b.low))
            .unwrap_or_else(|| "no reply".into())
    ));
    link.send(cmd::SYNC_MANIFEST, &[]).await?;
    let (manifest, _) = link.legacy_frames(cmd::SYNC_MANIFEST, Duration::from_secs(3), Duration::from_secs(5)).await?;
    for (c, p) in &manifest {
        found.push(format!("manifest       {} {}", legacy::name(*c), rwfit::hex(p)));
    }

    // Every stream the vendor knows, so a wrong feature bit cannot hide one.
    let streams = [cmd::STEPS, cmd::SLEEP, cmd::HEART_RATE, cmd::BLOOD_PRESSURE, cmd::SPO2, cmd::TEMPERATURE, cmd::BREATHING];
    for c in streams {
        link.send(c, &[]).await?;
        let first = link.legacy_frames(c, Duration::from_secs(5), Duration::from_secs(5)).await?.0;
        let mut frames = first;
        if !frames.is_empty() {
            frames.extend(link.legacy_frames(c, SETTLE, Duration::from_secs(60)).await?.0);
        }
        let payload: Vec<u8> = frames.iter().filter(|(fc, _)| *fc == c).flat_map(|(_, p)| p.clone()).collect();
        found.push(format!("{:<14} {}", legacy::name(c).trim_end_matches(" history"), summarise(c, &payload, frames.len())));
        if !payload.is_empty() {
            report.section(format!("{} (raw payload)", legacy::name(c)), vec![rwfit::hex(&payload)]);
        }
    }

    if services.contains(&HEART_RATE_SERVICE) {
        found.push(format!("standard HR    {}", heart_rate_service(&device, &report).await));
    }
    report.finding("RWfit ring", found.join("\n    "));
    Ok(())
}

fn summarise(c: u8, payload: &[u8], frames: usize) -> String {
    if frames == 0 {
        return "no reply".into();
    }
    match c {
        cmd::STEPS => {
            let days = records::steps(payload);
            let list: Vec<String> = days
                .iter()
                .map(|d| format!("{} {} steps ({} slots)", local_time(d.local).get(..10).unwrap_or_default(), d.steps, d.slots.len()))
                .collect();
            format!("{} frames, {} days: {}", frames, days.len(), list.join("; "))
        }
        cmd::SLEEP => {
            let nights = records::sleep(payload);
            let list: Vec<String> = nights
                .iter()
                .map(|n| {
                    format!("{} to {}, {} min in {} stages", local_time(n.asleep), local_time(n.awake), n.total_minutes, n.stages.len())
                })
                .collect();
            format!("{} frames, {} nights: {}", frames, nights.len(), list.join("; "))
        }
        _ => {
            let s = records::series(c, payload);
            let (Some(first), Some(last)) = (s.first(), s.last()) else { return format!("{frames} frames, no readings") };
            let (lo, hi) = s.iter().fold((f64::MAX, f64::MIN), |(lo, hi), x| (lo.min(x.value), hi.max(x.value)));
            format!(
                "{frames} frames, {} readings from {} to {}, {lo:.1} to {hi:.1}",
                s.len(),
                local_time(first.local),
                local_time(last.local)
            )
        }
    }
}

/// Listens to the standard Heart Rate Measurement characteristic for 30 s.
async fn heart_rate_service(device: &Device, report: &Report) -> String {
    let result: Result<String> = async {
        for s in device.services().await? {
            if s.uuid().await? != HEART_RATE_SERVICE {
                continue;
            }
            for c in s.characteristics().await? {
                if c.uuid().await? != HEART_RATE_MEASUREMENT {
                    continue;
                }
                report.note("listening to the standard Heart Rate Measurement characteristic for 30 s");
                let stream = c.notify().await?;
                pin_mut!(stream);
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut values = Vec::new();
                while let Ok(Some(v)) = timeout(deadline.saturating_duration_since(Instant::now()), stream.next()).await {
                    report.rx(&v, "standard heart rate");
                    // Flags bit 0 picks an 8- or 16-bit value.
                    let bpm = if v.first().is_some_and(|f| f & 1 != 0) {
                        v.get(1..3).map(|b| u16::from_le_bytes([b[0], b[1]]))
                    } else {
                        v.get(1).map(|b| *b as u16)
                    };
                    values.extend(bpm);
                }
                return Ok(if values.is_empty() {
                    "no notifications in 30 s".into()
                } else {
                    format!("{} readings: {:?}", values.len(), values)
                });
            }
        }
        Err(anyhow!("measurement characteristic not found"))
    }
    .await;
    result.unwrap_or_else(|e| format!("failed: {e:#}"))
}
