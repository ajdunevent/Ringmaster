//! BlueZ access through bluer, with the FuriOS/BlueZ 5.85 behaviour found in Lithidion built in.

use crate::protocol::{self, COMMAND_CHAR, DATA_SERVICE, NOTIFY_CHAR};
use crate::report::Report;
use anyhow::{anyhow, Context, Result};
use bluer::agent::Agent;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, UuidExt};
use futures::{pin_mut, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

/// Accepts every pairing prompt. Leaving `request_confirmation` as `None` makes BlueZ reject
/// pairing ("User Confirmation Negative Reply").
pub fn agent(report: Arc<Report>) -> Agent {
    let (r1, r2, r3) = (report.clone(), report.clone(), report);
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(move |req| {
            let r = r1.clone();
            Box::pin(async move {
                r.note(format!("agent: confirmation requested by {} (passkey {:06}), accepted", req.device, req.passkey));
                Ok(())
            })
        })),
        request_authorization: Some(Box::new(move |req| {
            let r = r2.clone();
            Box::pin(async move {
                r.note(format!("agent: authorization requested by {}, accepted", req.device));
                Ok(())
            })
        })),
        authorize_service: Some(Box::new(move |req| {
            let r = r3.clone();
            Box::pin(async move {
                r.note(format!("agent: service {} authorization requested by {}, accepted", req.service, req.device));
                Ok(())
            })
        })),
        ..Default::default()
    }
}

pub async fn is_ring(device: &Device) -> bool {
    let name = device.name().await.ok().flatten().unwrap_or_default();
    let uuids = device.uuids().await.ok().flatten().unwrap_or_default();
    name.starts_with("RingConn") || uuids.contains(&DATA_SERVICE)
}

/// `device_addresses()` fails transiently right after the adapter is powered.
pub async fn known_addresses(adapter: &Adapter) -> Result<Vec<Address>> {
    let mut last = None;
    for _ in 0..3 {
        match adapter.device_addresses().await {
            Ok(a) => return Ok(a),
            Err(e) => last = Some(e),
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("listing known devices failed: {}", last.map(|e| e.to_string()).unwrap_or_default()))
}

#[derive(Debug, Clone)]
pub struct Sighting {
    pub address: Address,
    pub lines: Vec<String>,
    /// Signal strength of a current advertisement; `None` for a device known only from BlueZ's cache.
    pub rssi: Option<i16>,
}

async fn describe(device: &Device) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("address        {}", device.address()));
    if let Ok(t) = device.address_type().await {
        lines.push(format!("address type   {t}"));
    }
    if let Ok(Some(n)) = device.name().await {
        lines.push(format!("name           {n}"));
    }
    if let Ok(Some(r)) = device.rssi().await {
        lines.push(format!("rssi           {r} dBm"));
    }
    if let Ok(Some(p)) = device.tx_power().await {
        lines.push(format!("tx power       {p} dBm"));
    }
    if let Ok(Some(u)) = device.uuids().await {
        let mut u: Vec<_> = u.into_iter().map(|u| u.to_string()).collect();
        u.sort();
        lines.push(format!("service uuids  {}", u.join(", ")));
    }
    if let Ok(Some(m)) = device.manufacturer_data().await {
        for (id, data) in m {
            lines.push(format!("manufacturer   0x{id:04x}: {}", protocol::hex(&data)));
        }
    }
    if let Ok(Some(s)) = device.service_data().await {
        for (id, data) in s {
            lines.push(format!("service data   {id}: {}", protocol::hex(&data)));
        }
    }
    lines
}

/// Scans until `max` elapses, or `linger` after the first ring is seen.
pub async fn scan(adapter: &Adapter, report: &Report, max: Duration, linger: Duration, want: Option<Address>) -> Result<Vec<Sighting>> {
    // Returns "Operation already in progress" on FLX1s; discovery works without the filter.
    if let Err(e) = adapter.set_discovery_filter(DiscoveryFilter { transport: DiscoveryTransport::Le, ..Default::default() }).await {
        report.note(format!("LE discovery filter not set ({e}); continuing without it"));
    }

    // StartDiscovery fails transiently with "Operation already in progress" on FLX1s.
    let mut stream = None;
    let mut last = None;
    for _ in 0..5 {
        match adapter.discover_devices().await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                last = Some(e);
                sleep(Duration::from_millis(1500)).await;
            }
        }
    }
    let stream = stream.ok_or_else(|| anyhow!("discovery would not start: {}", last.map(|e| e.to_string()).unwrap_or_default()))?;
    pin_mut!(stream);

    // BlueZ reports known devices as added as soon as discovery starts, from its cache; only an
    // RSSI shows the ring is advertising now.
    let mut found: HashMap<Address, Sighting> = HashMap::new();
    let mut deadline = Instant::now() + max;
    let shorten = |deadline: &mut Instant| *deadline = (*deadline).min(Instant::now() + linger);
    loop {
        for s in found.values_mut().filter(|s| s.rssi.is_none()) {
            let Ok(device) = adapter.device(s.address) else { continue };
            if let Ok(Some(rssi)) = device.rssi().await {
                report.note(format!("{} now advertising at {rssi} dBm", s.address));
                s.rssi = Some(rssi);
                s.lines = describe(&device).await;
                shorten(&mut deadline);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let evt = match timeout(left.min(Duration::from_millis(500)), stream.next()).await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => continue,
        };
        let AdapterEvent::DeviceAdded(addr) = evt else { continue };
        if found.contains_key(&addr) {
            continue;
        }
        let Ok(device) = adapter.device(addr) else { continue };
        let matches = match want {
            Some(w) => w == addr,
            None => is_ring(&device).await,
        };
        if !matches {
            continue;
        }
        let rssi = device.rssi().await.ok().flatten();
        match rssi {
            Some(r) => {
                report.note(format!("seen {addr} advertising at {r} dBm"));
                shorten(&mut deadline);
            }
            None => report.note(format!("{addr} known to BlueZ but not advertising yet")),
        }
        found.insert(addr, Sighting { address: addr, lines: describe(&device).await, rssi });
    }
    Ok(found.into_values().collect())
}

/// `connect()` may never resolve on FLX1s although the link comes up a few seconds later, so the
/// connection state is polled.
pub async fn connect(device: &Device, report: &Report) -> Result<()> {
    if device.is_connected().await.unwrap_or(false) {
        report.note("already connected");
        return Ok(());
    }
    report.note("connecting");
    match timeout(Duration::from_secs(3), device.connect()).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(e)) => report.note(format!("connect() returned an error: {e}; polling connection state")),
        Err(_) => report.note("connect() did not return within 3 s; polling connection state"),
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if device.is_connected().await.unwrap_or(false) {
            report.note("connected");
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("not connected after 20 s; seat the ring in its case for a moment, take it out and run again"))
}

/// `pair()` may never resolve on FLX1s although bonding completes, so the bond state is polled
/// alongside it. Returns what happened, for the report.
pub async fn pair(device: &Device) -> String {
    let started = Instant::now();
    let pairing = device.pair();
    pin_mut!(pairing);
    while started.elapsed() < Duration::from_secs(40) {
        tokio::select! {
            result = &mut pairing => {
                return match result {
                    Ok(()) => format!("pair() returned success after {:.1} s", started.elapsed().as_secs_f64()),
                    Err(e) => format!("pair() returned an error after {:.1} s: {e}", started.elapsed().as_secs_f64()),
                };
            }
            _ = sleep(Duration::from_millis(500)) => {
                if device.is_paired().await.unwrap_or(false) {
                    return format!("bonded after {:.1} s while pair() was still pending", started.elapsed().as_secs_f64());
                }
            }
        }
    }
    "no bond within 40 s".into()
}

/// Waits until the ring is connected and its services can be read. Bonding makes BlueZ re-read
/// the services, and reading them before that finishes fails with "GATT services have not been
/// resolved for that Bluetooth device".
pub async fn wait_ready(device: &Device, report: &Report, limit: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if !device.is_connected().await.unwrap_or(false) {
            report.note("link dropped; reconnecting");
            let _ = connect(device, report).await;
        }
        if device.is_services_resolved().await.unwrap_or(false) && device.services().await.is_ok_and(|s| !s.is_empty()) {
            report.note(format!("services readable after {:.1} s", started.elapsed().as_secs_f64()));
            return true;
        }
        sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Every service, characteristic and descriptor, with values of readable characteristics in the
/// standard GAP (0x1800), Device Information (0x180a) and Battery (0x180f) services.
pub async fn gatt_table(device: &Device) -> Result<(Vec<String>, Vec<(String, Vec<u8>)>)> {
    let mut lines = Vec::new();
    let mut values = Vec::new();
    let readable_services = [0x1800u16, 0x180a, 0x180f];
    for service in device.services().await? {
        let su = service.uuid().await?;
        let primary = service.primary().await.unwrap_or(false);
        lines.push(format!("service {su}{}", if primary { "" } else { " (secondary)" }));
        let std_service = su.as_u16().is_some_and(|s| readable_services.contains(&s));
        for ch in service.characteristics().await? {
            let cu = ch.uuid().await?;
            let flags = ch.flags().await?;
            let mut f = Vec::new();
            for (on, name) in [
                (flags.read, "read"),
                (flags.write, "write"),
                (flags.write_without_response, "write-no-resp"),
                (flags.notify, "notify"),
                (flags.indicate, "indicate"),
                (flags.encrypt_read || flags.encrypt_write, "encrypt"),
                (flags.encrypt_authenticated_read || flags.encrypt_authenticated_write, "encrypt-auth"),
            ] {
                if on {
                    f.push(name);
                }
            }
            lines.push(format!("  char {cu}  [{}]  handle 0x{:04x}", f.join(", "), ch.id()));
            for d in ch.descriptors().await.unwrap_or_default() {
                if let Ok(du) = d.uuid().await {
                    lines.push(format!("    desc {du}"));
                }
            }
            if std_service && flags.read {
                match ch.read().await {
                    Ok(v) => {
                        lines.push(format!("    value {}", printable(&v)));
                        values.push((short_uuid(cu), v));
                    }
                    Err(e) => lines.push(format!("    read failed: {e}")),
                }
            }
        }
    }
    Ok((lines, values))
}

pub fn short_uuid(u: bluer::Uuid) -> String {
    match u.as_u16() {
        Some(s) => format!("0x{s:04x}"),
        None => u.to_string(),
    }
}

pub fn printable(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) if !s.is_empty() && s.chars().all(|c| !c.is_control() || c == '\0') => {
            format!("\"{}\"  ({})", s.trim_end_matches('\0'), protocol::hex(v))
        }
        _ => protocol::hex(v),
    }
}

/// The command/notify pair. Every received frame is logged as it arrives and heartbeats are
/// answered from the receiving task, so they are acknowledged while the probe is waiting.
pub struct Link {
    cmd: Characteristic,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    report: Arc<Report>,
    last_send: Option<Instant>,
    /// False once the notification stream has ended. BlueZ can reconnect a trusted ring by
    /// itself afterwards, and writes then succeed while nothing comes back.
    open: bool,
    reader: tokio::task::AbortHandle,
}

/// A reader left running keeps bluer's notification session alive, and bluer then hands that
/// session to the next link without enabling notifications on the ring again.
impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Normal draining needs about 8 writes/s.
const MIN_SEND_GAP: Duration = Duration::from_millis(25);

impl Link {
    pub async fn open(device: &Device, report: Arc<Report>) -> Result<Self> {
        let mut cmd = None;
        let mut notify = None;
        for service in device.services().await? {
            if service.uuid().await? != DATA_SERVICE {
                continue;
            }
            for ch in service.characteristics().await? {
                match ch.uuid().await? {
                    u if u == COMMAND_CHAR => cmd = Some(ch),
                    u if u == NOTIFY_CHAR => notify = Some(ch),
                    _ => {}
                }
            }
        }
        let cmd = cmd.context("command characteristic 8327ad98 not found")?;
        let notify = notify.context("notify characteristic 8327ad97 not found")?;

        let stream = notify.notify().await.context("enabling notifications failed")?;
        report.note("notifications enabled on 8327ad97");
        let (tx, rx) = mpsc::unbounded_channel();
        let (r, hb_cmd) = (report.clone(), cmd.clone());
        let reader = tokio::spawn(async move {
            pin_mut!(stream);
            while let Some(frame) = stream.next().await {
                let op = frame.first().copied().unwrap_or(0);
                let check = match protocol::checksum_ok(&frame) {
                    Some(false) => "  BAD XOR",
                    _ => "",
                };
                r.rx(&frame, &format!("{}{check}", protocol::frame_name(op)));
                if op == 0x11 {
                    r.tx(&protocol::cmd::HEARTBEAT_ACK, "heartbeat ack (auto)");
                    let _ = hb_cmd.write_ext(&protocol::cmd::HEARTBEAT_ACK, &write_request()).await;
                }
                if tx.send(frame).is_err() {
                    break;
                }
            }
            r.note("notification stream ended");
        });
        Ok(Self { cmd, rx, report, last_send: None, open: true, reader: reader.abort_handle() })
    }

    pub async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(t) = self.last_send {
            tokio::time::sleep_until(t + MIN_SEND_GAP).await;
        }
        self.last_send = Some(Instant::now());
        let op = bytes.first().copied().unwrap_or(0);
        self.report.tx(bytes, protocol::command_name(op));
        self.cmd.write_ext(bytes, &write_request()).await.with_context(|| format!("write {} failed", protocol::hex(bytes)))
    }

    pub async fn recv(&mut self, limit: Duration) -> Option<Vec<u8>> {
        match timeout(limit, self.rx.recv()).await {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => {
                self.open = false;
                // The channel is closed, so a caller looping on `recv` would otherwise spin.
                sleep(limit).await;
                None
            }
            Err(_) => None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// First frame whose opcode is in `ops`; other frames are logged and dropped.
    pub async fn wait_for(&mut self, ops: &[u8], limit: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let f = self.recv(left).await?;
            if f.first().is_some_and(|op| ops.contains(op)) {
                return Some(f);
            }
        }
    }

    /// All frames until `quiet` passes with nothing new, or `max` elapses.
    pub async fn collect(&mut self, quiet: Duration, max: Duration) -> Vec<Vec<u8>> {
        let deadline = Instant::now() + max;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now()).min(quiet);
            if left.is_zero() {
                return out;
            }
            match self.recv(left).await {
                Some(f) => out.push(f),
                None => return out,
            }
        }
    }

    pub fn command_char(&self) -> Characteristic {
        self.cmd.clone()
    }
}

pub fn write_request() -> CharacteristicWriteRequest {
    CharacteristicWriteRequest { op_type: WriteOp::Request, ..Default::default() }
}
