//! BlueZ access through bluer, with the FuriOS/BlueZ 5.85 behaviour found in Lithidion and
//! ringprobe built in.

use anyhow::{anyhow, Context, Result};
use bluer::agent::Agent;
use bluer::gatt::remote::{Characteristic, CharacteristicWriteRequest};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport, UuidExt};
use futures::{pin_mut, StreamExt};
use ringconn::frame::{cmd, op};
use ringconn::{COMMAND_CHAR, DATA_SERVICE, NOTIFY_CHAR};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

/// Accepts every pairing prompt; the ring pairs with Just Works. Leaving `request_confirmation`
/// as `None` makes BlueZ reject pairing ("User Confirmation Negative Reply").
pub fn agent() -> Agent {
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(|req| {
            Box::pin(async move {
                tracing::info!(device = %req.device, "pairing confirmation accepted");
                Ok(())
            })
        })),
        request_authorization: Some(Box::new(|req| {
            Box::pin(async move {
                tracing::info!(device = %req.device, "pairing authorization accepted");
                Ok(())
            })
        })),
        authorize_service: Some(Box::new(|_| Box::pin(async { Ok(()) }))),
        ..Default::default()
    }
}

pub async fn is_ring(device: &Device) -> bool {
    let name = device.name().await.ok().flatten().unwrap_or_default();
    let uuids = device.uuids().await.ok().flatten().unwrap_or_default();
    name.starts_with("RingConn") || uuids.contains(&DATA_SERVICE) || is_colmi_with(&name, &uuids)
}

pub async fn is_colmi(device: &Device) -> bool {
    let name = device.name().await.ok().flatten().unwrap_or_default();
    let uuids = device.uuids().await.ok().flatten().unwrap_or_default();
    is_colmi_with(&name, &uuids)
}

fn is_colmi_with(name: &str, uuids: &std::collections::HashSet<bluer::Uuid>) -> bool {
    colmi::is_colmi(name, &uuids.iter().copied().collect::<Vec<_>>())
}

/// Rings BlueZ knows, bonded ones first. `device_addresses()` fails transiently right after the
/// adapter is powered.
pub async fn known_rings(adapter: &Adapter) -> Result<Vec<Address>> {
    let mut last = None;
    let mut addresses = None;
    for _ in 0..3 {
        match adapter.device_addresses().await {
            Ok(a) => {
                addresses = Some(a);
                break;
            }
            Err(e) => last = Some(e),
        }
        sleep(Duration::from_millis(500)).await;
    }
    let addresses =
        addresses.ok_or_else(|| anyhow!("listing known devices failed: {}", last.map(|e| e.to_string()).unwrap_or_default()))?;
    let mut rings = Vec::new();
    for addr in addresses {
        let Ok(d) = adapter.device(addr) else { continue };
        if is_ring(&d).await {
            rings.push((d.is_paired().await.unwrap_or(false), addr));
        }
    }
    rings.sort_by_key(|(paired, _)| !paired);
    Ok(rings.into_iter().map(|(_, a)| a).collect())
}

/// First ring seen advertising, or `want` if given, with its signal strength.
pub async fn scan(adapter: &Adapter, limit: Duration, want: Option<Address>) -> Result<Option<(Address, i16)>> {
    // Returns "Operation already in progress" on FLX1s; discovery works without the filter.
    if let Err(e) = adapter.set_discovery_filter(DiscoveryFilter { transport: DiscoveryTransport::Le, ..Default::default() }).await {
        tracing::debug!(error = %e, "LE discovery filter not set");
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
    let deadline = Instant::now() + limit;
    let mut candidates: Vec<Device> = Vec::new();
    loop {
        for device in &candidates {
            if let Ok(Some(rssi)) = device.rssi().await {
                return Ok(Some((device.address(), rssi)));
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let evt = match timeout(left.min(Duration::from_millis(500)), stream.next()).await {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(None),
            Err(_) => continue,
        };
        let AdapterEvent::DeviceAdded(addr) = evt else { continue };
        let Ok(device) = adapter.device(addr) else { continue };
        let found = match want {
            Some(w) => w == addr,
            None => is_ring(&device).await,
        };
        if found {
            candidates.push(device);
        }
    }
}

/// Every ring advertising during `limit` except those in `skip`, strongest signal first, as
/// (address, name, RSSI).
pub async fn scan_all(adapter: &Adapter, limit: Duration, skip: &[Address]) -> Result<Vec<(Address, String, i16)>> {
    let mut found: Vec<(Address, String, i16)> = Vec::new();
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let seen: Vec<Address> = skip.iter().copied().chain(found.iter().map(|f| f.0)).collect();
        let Some((addr, rssi)) = scan_excluding(adapter, left, &seen).await? else { break };
        let name = adapter.device(addr).ok().map(|d| async move { d.name().await.ok().flatten() });
        let name = match name {
            Some(f) => f.await.unwrap_or_else(|| addr.to_string()),
            None => addr.to_string(),
        };
        found.push((addr, name, rssi));
    }
    found.sort_by_key(|f| -f.2);
    Ok(found)
}

async fn scan_excluding(adapter: &Adapter, limit: Duration, skip: &[Address]) -> Result<Option<(Address, i16)>> {
    if let Err(e) = adapter.set_discovery_filter(DiscoveryFilter { transport: DiscoveryTransport::Le, ..Default::default() }).await {
        tracing::debug!(error = %e, "LE discovery filter not set");
    }
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
    let deadline = Instant::now() + limit;
    let mut candidates: Vec<Device> = Vec::new();
    loop {
        for device in &candidates {
            if let Ok(Some(rssi)) = device.rssi().await {
                return Ok(Some((device.address(), rssi)));
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let evt = match timeout(left.min(Duration::from_millis(500)), stream.next()).await {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(None),
            Err(_) => continue,
        };
        let AdapterEvent::DeviceAdded(addr) = evt else { continue };
        if skip.contains(&addr) {
            continue;
        }
        let Ok(device) = adapter.device(addr) else { continue };
        if is_ring(&device).await {
            candidates.push(device);
        }
    }
}

/// `connect()` may never resolve on FLX1s although the link comes up a few seconds later, so the
/// connection state is polled.
pub async fn connect(device: &Device) -> Result<()> {
    if device.is_connected().await.unwrap_or(false) {
        return Ok(());
    }
    match timeout(Duration::from_secs(3), device.connect()).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(e)) => tracing::debug!(error = %e, "connect() failed; polling connection state"),
        Err(_) => tracing::debug!("connect() did not return; polling connection state"),
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if device.is_connected().await.unwrap_or(false) {
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("the ring did not connect; seat it in its case for a moment, take it out and try again"))
}

/// `pair()` may never resolve on FLX1s although bonding completes, so the bond state is polled
/// alongside it.
pub async fn pair(device: &Device) -> Result<()> {
    let pairing = device.pair();
    pin_mut!(pairing);
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        tokio::select! {
            result = &mut pairing => {
                if let Err(e) = result {
                    tracing::warn!(error = %e, "pair() returned an error");
                }
                break;
            }
            _ = sleep(Duration::from_millis(500)) => {
                if device.is_paired().await.unwrap_or(false) {
                    break;
                }
            }
        }
    }
    if device.is_paired().await.unwrap_or(false) {
        Ok(())
    } else {
        // Pairing is cancelled while the ring sits in its closed case.
        Err(anyhow!("pairing failed; seat the ring in its case for a moment, take it out and try again"))
    }
}

/// Waits until the ring is connected and its services can be read. Bonding makes BlueZ re-read
/// the services, and reading them before that finishes fails with "GATT services have not been
/// resolved for that Bluetooth device".
pub async fn wait_ready(device: &Device) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !device.is_connected().await.unwrap_or(false) {
            let _ = connect(device).await;
        }
        if device.is_services_resolved().await.unwrap_or(false) && device.services().await.is_ok_and(|s| !s.is_empty()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("the ring's services could not be read; try connecting again"));
        }
        sleep(Duration::from_millis(500)).await;
    }
}

/// Device Information Service values: (serial, firmware, hardware).
pub async fn device_information(device: &Device) -> (Option<String>, Option<String>, Option<String>) {
    let (mut serial, mut firmware, mut hardware) = (None, None, None);
    let Ok(services) = device.services().await else { return (None, None, None) };
    for service in services {
        if service.uuid().await.ok().and_then(|u| u.as_u16()) != Some(0x180a) {
            continue;
        }
        for ch in service.characteristics().await.unwrap_or_default() {
            let Some(id) = ch.uuid().await.ok().and_then(|u| u.as_u16()) else { continue };
            let slot = match id {
                0x2a25 => &mut serial,
                0x2a26 => &mut firmware,
                0x2a27 => &mut hardware,
                _ => continue,
            };
            if let Ok(v) = ch.read().await {
                *slot = Some(String::from_utf8_lossy(&v).trim_end_matches('\0').to_string());
            }
        }
    }
    (serial, firmware, hardware)
}

pub enum Received {
    Frame(Vec<u8>),
    /// From the second notify characteristic, which only Colmi rings have.
    Data(Vec<u8>),
    Quiet,
    Closed,
}

/// The command/notify pair, plus the big-data pair on Colmi rings. RingConn heartbeats are
/// answered from the receiving task so they are acknowledged whatever the backend is doing.
pub struct Link {
    cmd: (Characteristic, WriteOp),
    data: Option<(Characteristic, WriteOp)>,
    rx: mpsc::UnboundedReceiver<(bool, Vec<u8>)>,
    last_send: Option<Instant>,
    readers: Vec<tokio::task::AbortHandle>,
}

/// BlueZ can drop a link without ending its notify stream. A reader left running keeps bluer's
/// notification session alive, and bluer then hands that session to the next link without
/// enabling notifications on the ring again, so the new link hears nothing.
impl Drop for Link {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
    }
}

/// Normal draining needs about 8 writes/s.
const MIN_SEND_GAP: Duration = Duration::from_millis(25);

/// Characteristics of `service` named in `wanted`, in that order.
async fn characteristics(device: &Device, service: bluer::Uuid, wanted: &[bluer::Uuid]) -> Result<Vec<Option<Characteristic>>> {
    let mut found = vec![None; wanted.len()];
    for s in device.services().await? {
        if s.uuid().await? != service {
            continue;
        }
        for ch in s.characteristics().await? {
            let uuid = ch.uuid().await?;
            if let Some(i) = wanted.iter().position(|w| *w == uuid) {
                found[i] = Some(ch);
            }
        }
    }
    Ok(found)
}

/// Colmi rings offer only write without response on some characteristics.
async fn write_op(ch: &Characteristic) -> WriteOp {
    match ch.flags().await {
        Ok(f) if !f.write && f.write_without_response => WriteOp::Command,
        _ => WriteOp::Request,
    }
}

fn spawn_reader(
    stream: impl futures::Stream<Item = Vec<u8>> + Send + 'static,
    tx: mpsc::UnboundedSender<(bool, Vec<u8>)>,
    data: bool,
    heartbeat: Option<Characteristic>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        pin_mut!(stream);
        while let Some(frame) = stream.next().await {
            tracing::trace!(frame = %ringconn::frame::hex(&frame), data, "rx");
            if let (Some(hb), Some(&op::HEARTBEAT)) = (&heartbeat, frame.first()) {
                let _ = hb.write_ext(&cmd::HEARTBEAT_ACK, &write_request(WriteOp::Request)).await;
            }
            if tx.send((data, frame)).is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

fn write_request(op_type: WriteOp) -> CharacteristicWriteRequest {
    CharacteristicWriteRequest { op_type, ..Default::default() }
}

impl Link {
    pub async fn open(device: &Device) -> Result<Self> {
        let mut found = characteristics(device, DATA_SERVICE, &[COMMAND_CHAR, NOTIFY_CHAR]).await?.into_iter();
        let command = found.next().flatten().context("this device has no RingConn command characteristic")?;
        let notify = found.next().flatten().context("this device has no RingConn notify characteristic")?;
        let stream = notify.notify().await.context("enabling notifications failed")?;
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = spawn_reader(stream, tx, false, Some(command.clone()));
        Ok(Self { cmd: (command, WriteOp::Request), data: None, rx, last_send: None, readers: vec![reader] })
    }

    /// Colmi rings: UART-style commands, and big-data requests on a second service when the
    /// model has it.
    pub async fn open_colmi(device: &Device) -> Result<Self> {
        let mut uart = characteristics(device, colmi::UART_SERVICE, &[colmi::UART_WRITE, colmi::UART_NOTIFY]).await?.into_iter();
        let write = uart.next().flatten().context("this device has no Colmi write characteristic")?;
        let notify = uart.next().flatten().context("this device has no Colmi notify characteristic")?;
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = notify.notify().await.context("enabling notifications failed")?;
        let mut readers = vec![spawn_reader(stream, tx.clone(), false, None)];
        let mut big = characteristics(device, colmi::DATA_SERVICE, &[colmi::DATA_WRITE, colmi::DATA_NOTIFY]).await?.into_iter();
        let data = match (big.next().flatten(), big.next().flatten()) {
            (Some(w), Some(n)) => match n.notify().await {
                Ok(stream) => {
                    readers.push(spawn_reader(stream, tx, true, None));
                    let op = write_op(&w).await;
                    Some((w, op))
                }
                Err(e) => {
                    tracing::info!(error = %e, "Colmi big-data notifications could not be enabled");
                    None
                }
            },
            _ => None,
        };
        let op = write_op(&write).await;
        Ok(Self { cmd: (write, op), data, rx, last_send: None, readers })
    }

    pub fn has_data(&self) -> bool {
        self.data.is_some()
    }

    pub async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let (ch, op) = self.cmd.clone();
        self.write(&ch, op, bytes).await
    }

    /// Writes to the big-data characteristic.
    pub async fn send_data(&mut self, bytes: &[u8]) -> Result<()> {
        let (ch, op) = self.data.clone().context("this ring has no big-data characteristic")?;
        self.write(&ch, op, bytes).await
    }

    async fn write(&mut self, ch: &Characteristic, op: WriteOp, bytes: &[u8]) -> Result<()> {
        if let Some(t) = self.last_send {
            tokio::time::sleep_until(t + MIN_SEND_GAP).await;
        }
        self.last_send = Some(Instant::now());
        tracing::trace!(frame = %ringconn::frame::hex(bytes), "tx");
        ch.write_ext(bytes, &write_request(op)).await.with_context(|| format!("write {} failed", ringconn::frame::hex(bytes)))
    }

    pub async fn recv(&mut self, limit: Duration) -> Received {
        match timeout(limit, self.rx.recv()).await {
            Ok(Some((false, f))) => Received::Frame(f),
            Ok(Some((true, f))) => Received::Data(f),
            Ok(None) => Received::Closed,
            Err(_) => Received::Quiet,
        }
    }

    /// First frame with opcode `code`; other frames are passed to `other`.
    pub async fn wait_for(&mut self, code: u8, limit: Duration, mut other: impl FnMut(&[u8])) -> Option<Vec<u8>> {
        let deadline = Instant::now() + limit;
        loop {
            match self.recv(deadline.saturating_duration_since(Instant::now())).await {
                Received::Frame(f) if f.first() == Some(&code) => return Some(f),
                Received::Frame(f) | Received::Data(f) => other(&f),
                Received::Quiet | Received::Closed => return None,
            }
        }
    }
}
