//! `--ultrahuman`: a first look at an Ultrahuman Ring AIR. Finds the ring by its name or command
//! service, reads its state, sets its clock, reads the first and last record numbers, and reads
//! every stored record. Records stay on the ring after reading, so nothing is lost. Then, unless
//! `--skip-live`, it starts an exercise for 30 s to see live heart rate (the ring keeps a record of
//! it, one every 2 s), and listens for state
//! notifications. Commands that change settings (SpO2, airplane mode, factory reset) are never
//! sent.

use crate::report::Report;
use crate::transport;
use crate::Shared;
use anyhow::{bail, Context, Result};
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, Device};
use chrono::{Local, TimeZone};
use futures::{pin_mut, StreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};
use ultrahuman::{op, Record, Records, Response, State};

/// How long the ring may stay silent in the middle of sending records before it is asked again.
const RECORDS_QUIET: Duration = Duration::from_secs(8);
/// Requests for the rest of the records after the ring goes quiet early.
const RECORD_RETRIES: usize = 5;
const LIVE: Duration = Duration::from_secs(30);
/// Enabling notifications waits this long. Without a bond the ring left the request for response
/// notifications unanswered until D-Bus gave up after 2 minutes; 35 s is past BlueZ's own 30 s
/// ATT timeout, so BlueZ's error is the one reported.
const NOTIFY_LIMIT: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Response,
    State,
    Data,
    Other,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Response => "response",
            Source::State => "state",
            Source::Data => "live data",
            Source::Other => "86f63001 (unknown)",
        }
    }
}

fn local(unix: u32) -> String {
    Local.timestamp_opt(unix as i64, 0).single().map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| unix.to_string())
}

fn label(source: Source, bytes: &[u8]) -> String {
    match source {
        Source::Response => match bytes.first() {
            Some(&o) => format!("response: {}", op::name(o)),
            None => "response: empty".into(),
        },
        Source::State => match State::parse(bytes) {
            Some(s) => format!("state: battery {} %, current {}, state {:02x}, ring {} °C", s.battery, s.current, s.state, s.temperature),
            None => format!("state, {} bytes", bytes.len()),
        },
        s => s.label().into(),
    }
}

/// How long to keep scanning after the first ring is heard, so that every ring in range is listed.
const MORE_RINGS: Duration = Duration::from_secs(4);

/// Scans even when the address is known, as BlueZ forgets an unbonded device soon after it was
/// last seen. Only a ring heard now counts: BlueZ reports a bonded ring from its cache as soon as
/// discovery starts, with no RSSI, even when it is out of range. With several rings in range and
/// no address given, the user picks one.
async fn find(adapter: &Adapter, report: &Report, want: Option<Address>) -> Result<Device> {
    report.note(match want {
        Some(a) => format!("scanning up to 30 s for {a}"),
        None => "scanning up to 30 s for an Ultrahuman ring (name UH_..., or service 86f65000)".into(),
    });
    let stream = adapter.discover_devices().await.context("discovery would not start")?;
    pin_mut!(stream);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen: Vec<Device> = Vec::new();
    let mut heard: BTreeMap<Address, (i16, Device)> = BTreeMap::new();
    let mut first_heard: Option<Instant> = None;
    loop {
        for d in &seen {
            let Some(rssi) = d.rssi().await.ok().flatten() else { continue };
            let matched = match want {
                Some(a) => d.address() == a,
                None => {
                    let name = d.name().await.ok().flatten();
                    let uuids = d.uuids().await.ok().flatten().unwrap_or_default();
                    ultrahuman::is_ultrahuman(name.as_deref(), uuids.iter())
                }
            };
            if matched {
                heard.insert(d.address(), (rssi, d.clone()));
                first_heard.get_or_insert_with(Instant::now);
            }
        }
        let settled = want.is_some() || first_heard.is_some_and(|t| t.elapsed() >= MORE_RINGS);
        if (settled || Instant::now() >= deadline) && !heard.is_empty() {
            let (rssi, d) = choose(&heard).await?;
            let name = d.name().await.ok().flatten().unwrap_or_default();
            let mut lines =
                vec![format!("address        {}", d.address()), format!("name           {name}"), format!("rssi           {rssi} dBm")];
            for (k, v) in d.manufacturer_data().await.ok().flatten().unwrap_or_default() {
                lines.push(format!("manufacturer   {k:#06x}: {}", ultrahuman::hex(&v)));
            }
            for u in d.uuids().await.ok().flatten().unwrap_or_default() {
                lines.push(format!("service        {u}"));
            }
            lines.push(format!("bonded         {}", d.is_paired().await.unwrap_or(false)));
            for (a, (r, _)) in &heard {
                if *a != d.address() {
                    lines.push(format!("also heard     {a} at {r} dBm"));
                }
            }
            report.section("Advertisement", lines);
            report.note(format!("found {} \"{name}\" at {rssi} dBm", d.address()));
            return Ok(d);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("no Ultrahuman ring advertising; keep it close and run again");
        }
        // Devices are checked again on every pass: a name, and an RSSI, can arrive after the device.
        if let Ok(Some(AdapterEvent::DeviceAdded(a))) = timeout(left.min(Duration::from_millis(500)), stream.next()).await {
            if let Ok(d) = adapter.device(a) {
                if !seen.iter().any(|s| s.address() == a) {
                    seen.push(d);
                }
            }
        }
    }
}

/// The only ring heard, or the one the user picks from a numbered list.
async fn choose(heard: &BTreeMap<Address, (i16, Device)>) -> Result<(i16, Device)> {
    let rings: Vec<(i16, Device)> = heard.values().cloned().collect();
    if rings.len() == 1 {
        return Ok(rings[0].clone());
    }
    let mut lines = Vec::new();
    for (i, (rssi, d)) in rings.iter().enumerate() {
        let name = d.name().await.ok().flatten().unwrap_or_default();
        let bonded = if d.is_paired().await.unwrap_or(false) { ", bonded" } else { "" };
        lines.push(format!("  {}  {}  \"{name}\"  {rssi} dBm{bonded}", i + 1, d.address()));
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("{} Ultrahuman rings are advertising; choose one with --address:\n{}", rings.len(), lines.join("\n"));
    }
    eprintln!("{} Ultrahuman rings are advertising:\n{}", rings.len(), lines.join("\n"));
    loop {
        eprint!("Which one? [1-{}] ", rings.len());
        let answer = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await??;
        if answer.is_empty() {
            bail!("no ring chosen");
        }
        if let Ok(n) = answer.trim().parse::<usize>() {
            if (1..=rings.len()).contains(&n) {
                return Ok(rings[n - 1].clone());
            }
        }
    }
}

/// A pairing agent that accepts the Just Works confirmation for `ring` and rejects anything else.
/// The ring pairs with LE Secure Connections and no input or output; BlueZ then asks an agent to
/// confirm, and with none registered it refuses the pairing itself ("No agent available for
/// request type 2").
fn agent(ring: Address, report: Arc<Report>) -> bluer::agent::Agent {
    use bluer::agent::{Agent, ReqError};
    let (r1, r2) = (report.clone(), report);
    Agent {
        request_default: false,
        request_confirmation: Some(Box::new(move |req| {
            let r = r1.clone();
            Box::pin(async move {
                if req.device == ring {
                    r.note(format!("agent: confirming pairing with {} (passkey {:06})", req.device, req.passkey));
                    Ok(())
                } else {
                    Err(ReqError::Rejected)
                }
            })
        })),
        request_authorization: Some(Box::new(move |req| {
            let r = r2.clone();
            Box::pin(async move {
                if req.device == ring {
                    r.note(format!("agent: authorising pairing with {}", req.device));
                    Ok(())
                } else {
                    Err(ReqError::Rejected)
                }
            })
        })),
        ..Default::default()
    }
}

/// Reads the state characteristic and describes it for the findings.
async fn read_state(device: &Device, report: &Report) -> String {
    let mut state = None;
    if let Ok(services) = device.services().await {
        for s in services {
            if s.uuid().await.ok() != Some(ultrahuman::STATE_SERVICE) {
                continue;
            }
            for c in s.characteristics().await.unwrap_or_default() {
                if c.uuid().await.ok() == Some(ultrahuman::STATE) {
                    state = Some(c);
                }
            }
        }
    }
    let Some(c) = state else { return "state read     86f61001 not found".into() };
    match timeout(Duration::from_secs(10), c.read()).await {
        Ok(Ok(v)) => {
            report.rx(&v, &label(Source::State, &v));
            format!(
                "state read     {} = {}",
                ultrahuman::hex(&v),
                State::parse(&v)
                    .map(|s| format!(
                        "battery {} %, {}, ring {} °C",
                        s.battery,
                        if s.charging() { "charging" } else { "not charging" },
                        s.temperature
                    ))
                    .unwrap_or_else(|| format!("{} bytes, not 7", v.len()))
            )
        }
        Ok(Err(e)) => format!("state read     failed: {e}"),
        Err(_) => "state read     no answer within 10 s".into(),
    }
}

/// The command characteristic and every notifying characteristic, with all notifications logged.
struct Link {
    command: Characteristic,
    request: bool,
    /// Whether notifications on the response characteristic could be enabled.
    responding: bool,
    rx: mpsc::UnboundedReceiver<(Source, Vec<u8>)>,
    report: Arc<Report>,
    readers: Vec<tokio::task::AbortHandle>,
    /// Every response frame, for the checksum test.
    responses: Vec<Vec<u8>>,
    /// Notifications that arrived while a response was awaited.
    states: Vec<Vec<u8>>,
    live: Vec<Vec<u8>>,
    other: Vec<Vec<u8>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        for r in &self.readers {
            r.abort();
        }
    }
}

impl Link {
    async fn open(device: &Device, report: Arc<Report>) -> Result<Self> {
        let mut found: BTreeMap<uuid::Uuid, Characteristic> = BTreeMap::new();
        for s in device.services().await? {
            for c in s.characteristics().await? {
                found.insert(c.uuid().await?, c);
            }
        }
        let command = found.get(&ultrahuman::COMMAND).cloned().context("command characteristic 86f65001 not found")?;
        let request = command.flags().await.map(|f| f.write).unwrap_or(true);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut readers = Vec::new();
        let mut responding = false;
        // The response characteristic comes last: it is the one that stalls.
        for (uuid, source) in [
            (ultrahuman::STATE, Source::State),
            (ultrahuman::DATA, Source::Data),
            (ultrahuman::OTHER, Source::Other),
            (ultrahuman::RESPONSE, Source::Response),
        ] {
            let Some(c) = found.get(&uuid) else {
                report.note(format!("{uuid} not found"));
                continue;
            };
            match timeout(NOTIFY_LIMIT, c.notify()).await.unwrap_or_else(|_| {
                Err(bluer::Error { kind: bluer::ErrorKind::Failed, message: format!("no answer within {} s", NOTIFY_LIMIT.as_secs()) })
            }) {
                Ok(stream) => {
                    responding |= source == Source::Response;
                    report.note(format!("notifications enabled on {} ({})", transport::short_uuid(uuid), source.label()));
                    let (tx, r) = (tx.clone(), report.clone());
                    let task = tokio::spawn(async move {
                        pin_mut!(stream);
                        while let Some(v) = stream.next().await {
                            r.rx(&v, &label(source, &v));
                            if tx.send((source, v)).is_err() {
                                break;
                            }
                        }
                        r.note(format!("{} notifications ended", source.label()));
                    });
                    readers.push(task.abort_handle());
                }
                Err(e) => report.note(format!("enabling notifications on {} failed: {e}", source.label())),
            }
        }
        report.note(format!("commands written {}", if request { "with response" } else { "without response" }));
        Ok(Self {
            command,
            request,
            responding,
            rx,
            report,
            readers,
            responses: Vec::new(),
            states: Vec::new(),
            live: Vec::new(),
            other: Vec::new(),
        })
    }

    async fn send(&self, bytes: &[u8]) -> Result<()> {
        let what = bytes.first().map(|o| op::name(*o)).unwrap_or("empty");
        self.report.tx(bytes, what);
        let req =
            CharacteristicWriteRequest { op_type: if self.request { WriteOp::Request } else { WriteOp::Command }, ..Default::default() };
        self.command.write_ext(bytes, &req).await.with_context(|| format!("writing {what} failed"))
    }

    /// The next notification of any kind within `limit`, filed by source.
    async fn next(&mut self, limit: Duration) -> Option<(Source, Vec<u8>)> {
        let (source, v) = timeout(limit, self.rx.recv()).await.ok().flatten()?;
        match source {
            Source::Response => self.responses.push(v.clone()),
            Source::State => self.states.push(v.clone()),
            Source::Data => self.live.push(v.clone()),
            Source::Other => self.other.push(v.clone()),
        }
        Some((source, v))
    }

    /// The first response to `operation` within `limit`.
    async fn response(&mut self, operation: u8, limit: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next(left).await {
                Some((Source::Response, v)) if v.first() == Some(&operation) => return Some(v),
                Some(_) => continue,
                None => return None,
            }
        }
    }

    async fn request(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>> {
        self.send(bytes).await?;
        Ok(self.response(bytes[0], Duration::from_secs(4)).await)
    }
}

fn describe(reply: &Option<Vec<u8>>) -> String {
    match reply.as_deref().and_then(Response::parse) {
        Some(r) => format!("{} ({})", ultrahuman::hex(reply.as_deref().unwrap_or_default()), if r.ok() { "ok" } else { "not ok" }),
        None => match reply {
            Some(v) => ultrahuman::hex(v),
            None => "no reply".into(),
        },
    }
}

pub async fn run(want: Option<Address>, live: bool, report: Arc<Report>, shared: Arc<Shared>) -> Result<()> {
    let session = bluer::Session::new().await.context("cannot reach BlueZ over D-Bus")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.context("cannot power the adapter")?;
    let device = find(&adapter, &report, want).await?;
    *shared.device.lock().unwrap() = Some(device.clone());
    // Kept for the whole run; dropping the handle unregisters the agent.
    let _agent = session.register_agent(agent(device.address(), report.clone())).await.context("cannot register a pairing agent")?;
    // On its charger the ring once refused the first attempt at once; later attempts are cheap.
    for attempt in 1..=3 {
        match transport::connect(&device, &report).await {
            Ok(()) => break,
            Err(e) if attempt < 3 => {
                report.note(format!("attempt {attempt} failed ({e}); trying again in 2 s"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
    if !transport::wait_ready(&device, &report, Duration::from_secs(30)).await {
        bail!("services not readable after 30 s");
    }
    if let Ok((lines, _)) = transport::gatt_table(&device).await {
        report.section("GATT layout", lines);
    }
    let mut found: Vec<String> = Vec::new();
    let bonded_before = device.is_paired().await.unwrap_or(false);
    // The key BlueZ shows as a company varies between rings (0x2d42, 0x2d31), so the whole
    // manufacturer data is kept.
    found.push(format!(
        "advertisement  {}",
        device
            .manufacturer_data()
            .await
            .ok()
            .flatten()
            .and_then(|m| m.iter().next().map(|(k, v)| format!("{k:#06x}: {}", ultrahuman::hex(v))))
            .unwrap_or_else(|| "no manufacturer data".into())
    ));

    // Reads work without a bond, so the state comes first.
    found.push(read_state(&device, &report).await);

    // Without a bond first, as Gadgetbridge does. The ring answers the request for response
    // notifications with Insufficient Authentication, so a bond is made when that fails.
    let mut link = Link::open(&device, report.clone()).await?;
    if link.responding {
        let paired = device.is_paired().await.unwrap_or(false);
        found.push(
            match (bonded_before, paired) {
                (true, _) => "bond           made before the run",
                (false, true) => "bond           made by BlueZ when response notifications asked for it, through the agent",
                (false, false) => "bond           none, and none needed",
            }
            .into(),
        );
    } else {
        drop(link);
        report.note("response notifications need a bond; pairing");
        let outcome = transport::pair(&device).await;
        report.note(&outcome);
        found.push(format!("bond           needed for response notifications; {outcome}"));
        if !device.is_paired().await.unwrap_or(false) {
            report.finding("Ultrahuman ring", found.join("\n    "));
            bail!("the ring did not pair");
        }
        if !transport::wait_ready(&device, &report, Duration::from_secs(30)).await {
            report.finding("Ultrahuman ring", found.join("\n    "));
            bail!("services not readable after pairing");
        }
        link = Link::open(&device, report.clone()).await?;
        if !link.responding {
            found.push("notifications  still off on 86f65002 after pairing".into());
            report.finding("Ultrahuman ring", found.join("\n    "));
            bail!("response notifications could not be enabled after pairing");
        }
    }
    let check = link.request(&[op::CHECK_DATA]).await?;
    if check.is_none() {
        found.push("check data     no answer".into());
        report.finding("Ultrahuman ring", found.join("\n    "));
        bail!("no answer to check data");
    }
    found.push(format!("check data     {}", describe(&check)));

    let now = chrono::Utc::now().timestamp() as u32;
    let time = link.request(&ultrahuman::set_time(now)).await?;
    found.push(format!("set time       {} to {}", describe(&time), local(now)));

    let first = link.request(&[op::FIRST_RECORD]).await?;
    let last = link.request(&[op::LAST_RECORD]).await?;
    let first_n = first.as_deref().and_then(Response::parse).and_then(|r| r.record_number());
    let last_n = last.as_deref().and_then(Response::parse).and_then(|r| r.record_number());
    found.push(format!("first record   {}", describe(&first)));
    found.push(format!("last record    {}", describe(&last)));

    let mut records: BTreeMap<u16, Record> = BTreeMap::new();
    if let (Some(f), Some(l)) = (first_n, last_n) {
        // A first number above the last means the numbers wrapped.
        let start = if f > l { 0 } else { f };
        let expected = (l - start) as usize + 1;
        report.note(format!("reading records {start} to {l} (about {expected})"));
        let started = Instant::now();
        let (mut requests, mut responses, mut from, mut ended) = (0usize, 0usize, start, "");
        while requests <= RECORD_RETRIES {
            link.send(&ultrahuman::records_from(from)).await?;
            requests += 1;
            let mut got_any = false;
            loop {
                let Some(v) = link.response(op::RECORDS, RECORDS_QUIET).await else { break };
                responses += 1;
                got_any = true;
                match ultrahuman::records(&v) {
                    Some(Records::Some(rs)) => {
                        let n = rs.len();
                        for r in rs {
                            records.insert(r.number, r);
                        }
                        if n < ultrahuman::RECORDS_PER_RESPONSE || records.contains_key(&l) {
                            ended = if n < ultrahuman::RECORDS_PER_RESPONSE { "a short response" } else { "the last record number" };
                            break;
                        }
                    }
                    Some(Records::Nothing) => {
                        ended = "status ee (nothing recorded)";
                        break;
                    }
                    Some(Records::Error { status, result }) => {
                        ended = "an error response";
                        report.note(format!("records response status {status:02x}, result {result:02x}"));
                        break;
                    }
                    None => {}
                }
            }
            if !ended.is_empty() || !got_any {
                break;
            }
            // Quiet before the end: ask for the rest, which shows whether one request is enough.
            from = records.keys().next_back().map_or(start, |n| n.wrapping_add(1));
            report.note(format!("ring went quiet after record {}; asking again from {from}", from.wrapping_sub(1)));
        }
        found.push(format!(
            "records        {} of about {expected} in {responses} responses to {requests} request(s), {:.1} s; ended by {}",
            records.len(),
            started.elapsed().as_secs_f64(),
            if ended.is_empty() { "silence" } else { ended }
        ));
        found.extend(summarise(&records));
    }
    if !records.is_empty() {
        report.section(
            "Records (number, optical time, HR, HRV, SpO2, kind, temp time, max, min, activity time, intensity, steps, stress, bytes 25 and 29)",
            records
                .values()
                .map(|r| {
                    format!(
                        "{:5}  {}  {:3} {:3} {:3}  {:3}  {}  {:5.2} {:5.2}  {}  {:3} {:5} {:3}  {:02x} {:02x}",
                        r.number,
                        local(r.optical_at),
                        r.heart_rate,
                        r.hrv,
                        r.spo2,
                        r.kind,
                        local(r.temperature_at),
                        r.temperature_max,
                        r.temperature_min,
                        local(r.activity_at),
                        r.intensity,
                        r.steps,
                        r.stress,
                        r.unknown[0],
                        r.unknown[1]
                    )
                })
                .collect(),
        );
    }

    if live {
        report.note(
            "starting an exercise for 30 s for live heart rate; wear the ring (it leaves about 10 exercise records in the ring's history)",
        );
        *shared.stop.lock().unwrap() = Some((link.command.clone(), vec![op::EXERCISE_STOP]));
        let before = link.live.len();
        let start = link.request(&[op::EXERCISE_START]).await?;
        let deadline = Instant::now() + LIVE;
        while link.next(deadline.saturating_duration_since(Instant::now())).await.is_some() {}
        // A ring with a failing battery can drop the link here; what was seen is still reported.
        let stop = match link.request(&[op::EXERCISE_STOP]).await {
            Ok(r) => {
                *shared.stop.lock().unwrap() = None;
                r
            }
            Err(e) => {
                let state = link.states.iter().rev().find_map(|v| State::parse(v));
                found.push(format!(
                    "link           lost during the exercise ({e}); last battery reading {}",
                    state.map_or("none".into(), |s| format!("{} %", s.battery))
                ));
                None
            }
        };
        let frames: Vec<&Vec<u8>> = link.live[before..].iter().collect();
        let live: Vec<ultrahuman::Live> = frames.iter().filter_map(|f| ultrahuman::Live::parse(f)).collect();
        let rates: Vec<u8> = live.iter().filter(|l| l.reading()).map(|l| l.heart_rate).collect();
        let without = live.len() - rates.len();
        let lengths: std::collections::BTreeSet<usize> = frames.iter().map(|f| f.len()).collect();
        let charging = link.states.iter().rev().find_map(|v| State::parse(v)).is_some_and(|s| s.charging());
        found.push(format!("exercise start {}{}", describe(&start), if charging { " (ring on its charger)" } else { "" }));
        found.push(format!(
            "live data      {} frames (lengths {lengths:?}); heart rates {rates:?}{}",
            frames.len(),
            if without > 0 { format!("; {without} without a reading (status 0a: off the finger?)") } else { String::new() }
        ));
        found.push(format!("exercise stop  {}", describe(&stop)));
        if !device.is_connected().await.unwrap_or(false) {
            report.finding("Ultrahuman ring", found.join("\n    "));
            bail!("the link dropped during the exercise");
        }
    }

    report.note("listening 20 s for state and other notifications");
    let deadline = Instant::now() + Duration::from_secs(20);
    while link.next(deadline.saturating_duration_since(Instant::now())).await.is_some() {}
    found.push(format!(
        "notifications  {} state, {} on 86f63001{}",
        link.states.len(),
        link.other.len(),
        link.other.first().map(|o| format!(" (first: {})", ultrahuman::hex(o))).unwrap_or_default()
    ));

    let failed: Vec<String> = link.responses.iter().filter(|v| !ultrahuman::check_ok(v)).map(|v| ultrahuman::hex(v)).collect();
    found.push(format!(
        "response check {} of {} responses pass CRC-16/CCITT-FALSE{}",
        link.responses.len() - failed.len(),
        link.responses.len(),
        failed.first().map(|f| format!("; first failure: {f}")).unwrap_or_default()
    ));
    report.finding("Ultrahuman ring", found.join("\n    "));
    Ok(())
}

/// Spacing, coverage and value ranges of the records, for deciding how to store and estimate sleep.
fn summarise(records: &BTreeMap<u16, Record>) -> Vec<String> {
    let rs: Vec<&Record> = records.values().collect();
    // Records stamped before the ring's clock was set carry seconds since it started instead.
    let dated: Vec<&Record> = rs.iter().copied().filter(|r| r.activity_at >= ultrahuman::CLOCK_SET_AFTER).collect();
    let undated = rs.len() - dated.len();
    let (Some(first), Some(last)) = (dated.iter().min_by_key(|r| r.activity_at), dated.iter().max_by_key(|r| r.activity_at)) else {
        return vec![format!("record times   all {undated} stamped before the clock was set")];
    };
    let mut times: Vec<u32> = dated.iter().map(|r| r.activity_at).collect();
    times.sort_unstable();
    let mut gaps: Vec<u32> = times.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    let median = gaps.get(gaps.len() / 2).copied().unwrap_or(0);
    let kinds = rs.iter().fold(BTreeMap::<u8, usize>::new(), |mut m, r| {
        *m.entry(r.kind).or_default() += 1;
        m
    });
    let worn = rs.iter().filter(|r| r.worn()).count();
    let range = |name: &str, v: Vec<f64>| -> String {
        if v.is_empty() {
            return format!("{name} none");
        }
        let (lo, hi) = v.iter().fold((f64::MAX, f64::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
        format!("{name} {} readings, {lo:.1} to {hi:.1}", v.len())
    };
    let nonzero = |f: fn(&Record) -> u8| -> Vec<f64> { rs.iter().map(|r| f(r)).filter(|v| *v != 0).map(f64::from).collect() };
    let temps: Vec<f64> =
        rs.iter().filter(|r| r.temperature_max != 0.0).flat_map(|r| [r.temperature_min as f64, r.temperature_max as f64]).collect();
    let skew = |a: fn(&Record) -> u32| rs.iter().map(|r| (a(r) as i64 - r.activity_at as i64).abs()).max().unwrap_or(0);
    vec![
        format!(
            "record times   {} to {}{}",
            local(first.activity_at),
            local(last.activity_at),
            if undated > 0 { format!("; {undated} stamped before the clock was set (seconds since start)") } else { String::new() }
        ),
        format!(
            "spacing        median {median} s, shortest {} s, longest {} s",
            gaps.first().copied().unwrap_or(0),
            gaps.last().copied().unwrap_or(0)
        ),
        format!(
            "time fields    optical up to {} s from activity, temperature up to {} s",
            skew(|r| r.optical_at),
            skew(|r| r.temperature_at)
        ),
        format!("kinds          {kinds:?}; {worn} of {} worn", rs.len()),
        format!(
            "values         {}; {}; {}",
            range("HR", nonzero(|r| r.heart_rate)),
            range("HRV", nonzero(|r| r.hrv)),
            range("SpO2", nonzero(|r| r.spo2))
        ),
        format!(
            "               {}; {}; {}; steps {}",
            range("skin °C", temps),
            range("intensity", nonzero(|r| r.intensity)),
            range("stress", nonzero(|r| r.stress)),
            rs.iter().map(|r| r.steps as u64).sum::<u64>()
        ),
        format!(
            "bytes 25, 29   {} and {} distinct values",
            rs.iter().map(|r| r.unknown[0]).collect::<std::collections::BTreeSet<_>>().len(),
            rs.iter().map(|r| r.unknown[1]).collect::<std::collections::BTreeSet<_>>().len()
        ),
    ]
}

/// How often the state is read during `--ultrahuman-state`, besides the ring's own notifications.
const STATE_EVERY: Duration = Duration::from_secs(30);

/// `--ultrahuman-state MINUTES`: battery, ring temperature, charging state and the unknown
/// bytes 1 to 4 over time, with no commands sent and no exercise started, so the run itself costs
/// the ring as little as possible. For telling a worn battery from one whose reading is skewed by
/// the heat of charging. The state characteristic needs no bond.
pub async fn watch_state(want: Option<Address>, minutes: u64, report: Arc<Report>, shared: Arc<Shared>) -> Result<()> {
    let session = bluer::Session::new().await.context("cannot reach BlueZ over D-Bus")?;
    let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
    adapter.set_powered(true).await.context("cannot power the adapter")?;
    let device = find(&adapter, &report, want).await?;
    *shared.device.lock().unwrap() = Some(device.clone());
    let started = Instant::now();
    let end = started + Duration::from_secs(minutes * 60);
    let mut rows: Vec<(f64, State)> = Vec::new();
    let mut drops = 0usize;
    report.note(format!("logging the state for {minutes} min"));
    'session: while Instant::now() < end {
        if !device.is_connected().await.unwrap_or(false) {
            if let Err(e) = transport::connect(&device, &report).await {
                report.note(format!("reconnecting failed ({e}); trying again in 10 s"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            if !transport::wait_ready(&device, &report, Duration::from_secs(30)).await {
                continue;
            }
        }
        let mut characteristic = None;
        for s in device.services().await.unwrap_or_default() {
            if s.uuid().await.ok() == Some(ultrahuman::STATE_SERVICE) {
                for c in s.characteristics().await.unwrap_or_default() {
                    if c.uuid().await.ok() == Some(ultrahuman::STATE) {
                        characteristic = Some(c);
                    }
                }
            }
        }
        let Some(c) = characteristic else { bail!("state characteristic 86f61001 not found") };
        let stream = timeout(NOTIFY_LIMIT, c.notify()).await.ok().and_then(|r| r.ok());
        let Some(stream) = stream else {
            report.note("state notifications could not be enabled; reading only");
            loop {
                if Instant::now() >= end {
                    break 'session;
                }
                match timeout(Duration::from_secs(10), c.read()).await {
                    Ok(Ok(v)) => record_state(&report, &mut rows, started, &v),
                    _ => {
                        drops += 1;
                        continue 'session;
                    }
                }
                tokio::time::sleep(STATE_EVERY).await;
            }
        };
        pin_mut!(stream);
        let mut next_read = Instant::now();
        loop {
            if Instant::now() >= end {
                break 'session;
            }
            if Instant::now() >= next_read {
                next_read = Instant::now() + STATE_EVERY;
                match timeout(Duration::from_secs(10), c.read()).await {
                    Ok(Ok(v)) => record_state(&report, &mut rows, started, &v),
                    _ => {
                        drops += 1;
                        report.note("state read failed; reconnecting");
                        continue 'session;
                    }
                }
            }
            let wait = next_read.min(end).saturating_duration_since(Instant::now());
            match timeout(wait, stream.next()).await {
                Ok(Some(v)) => record_state(&report, &mut rows, started, &v),
                Ok(None) => {
                    drops += 1;
                    report.note("state notifications ended; reconnecting");
                    continue 'session;
                }
                Err(_) => {}
            }
        }
    }
    report.section(
        "State over time (minutes, battery %, ring °C, state, bytes 1 to 4)",
        rows.iter()
            .map(|(t, s)| format!("{:7.2}  {:3}  {:3}  {:02x}  {}", t / 60.0, s.battery, s.temperature, s.state, s.current))
            .collect(),
    );
    let mut found =
        vec![format!("readings       {} over {:.1} min, {drops} reconnection(s)", rows.len(), started.elapsed().as_secs_f64() / 60.0)];
    if let (Some((_, a)), Some((_, b))) = (rows.first(), rows.last()) {
        found.push(format!("battery        {} % to {} %", a.battery, b.battery));
        found.push(format!("ring           {} °C to {} °C", a.temperature, b.temperature));
        let (lo, hi) = rows.iter().fold((u8::MAX, 0), |(lo, hi), (_, s)| (lo.min(s.battery), hi.max(s.battery)));
        found.push(format!("battery range  {lo} % to {hi} %"));
    }
    report.finding("Ultrahuman state", found.join("\n    "));
    Ok(())
}

fn record_state(report: &Report, rows: &mut Vec<(f64, State)>, started: Instant, v: &[u8]) {
    report.rx(v, &label(Source::State, v));
    if let Some(s) = State::parse(v) {
        rows.push((started.elapsed().as_secs_f64(), s));
    }
}
