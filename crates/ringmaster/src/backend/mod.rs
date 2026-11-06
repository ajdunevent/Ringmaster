//! Bluetooth and storage run on their own thread with a tokio runtime; the UI talks to it over
//! two channels, since GTK objects cannot cross threads.

mod colmi_ring;
mod transport;

use crate::settings::{Alert, Alerts, Rule, Settings, Stat};
pub use crate::store::MeasureKind;
use crate::store::{RingInfo, Spot, Store, Summary, SyncRecord};
use anyhow::{anyhow, bail, Context, Result};
use bluer::{Adapter, Address, Device};
use ringconn::drain::{Action, Batch, ChannelDrain, Outcome};
use ringconn::frame::{cmd, op};
use ringconn::records::{live_heart_rate, live_spo2, Descriptor};
use ringconn::Channel;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;
use transport::{Link, Received};

#[derive(Debug, Clone)]
pub enum Command {
    Connect,
    Disconnect,
    Sync,
    Measure(MeasureKind),
    StopMeasure,
    /// The window was shown or hidden.
    WindowVisible(bool),
    /// Settings or notification rules changed in the database.
    ReloadSettings,
    /// Removes the bond with the ring; its history stays.
    ForgetRing,
    /// Pairs with the ring chosen from `Event::ChooseRing` and makes it the default ring.
    ConnectTo(String),
    /// Looks for rings not yet known here and offers them with `Event::ChooseRing`.
    AddRing,
    /// Makes a known ring the one shown, synced and connected.
    SwitchRing(String),
    /// History changed outside a sync, as after an import; resends the summary.
    Refresh,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkState {
    Disconnected,
    Searching,
    Connecting,
    Pairing,
    Authenticating,
    Connected { name: String },
}

#[derive(Debug, Clone)]
pub enum Event {
    Link(LinkState),
    Telemetry(Descriptor),
    SyncStarted,
    SyncProgress {
        records: usize,
        remaining: u16,
    },
    SyncFinished {
        error: Option<String>,
    },
    MeasureStarted(MeasureKind),
    /// Latest reading, `None` until the ring locks on; progress through the window from 0 to 1.
    MeasureProgress {
        value: Option<u8>,
        progress: f64,
    },
    /// `result` is `None` when stopped or when too few readings arrived.
    MeasureFinished {
        kind: MeasureKind,
        result: Option<Spot>,
    },
    Summary(Box<Summary>),
    /// Battery of a ring that reports it outside status frames.
    Battery {
        percent: u8,
        charging: bool,
    },
    Notify(Alert),
    /// Rings not known here that are advertising; strongest signal first.
    ChooseRing(Vec<RingChoice>),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct RingChoice {
    pub address: String,
    pub name: String,
    pub rssi: i16,
}

pub fn spawn(db_path: PathBuf) -> (async_channel::Sender<Command>, async_channel::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = evt_tx.send_blocking(Event::Error(format!("cannot start the Bluetooth thread: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            match Store::open(&db_path) {
                Ok(store) => Backend::new(store, evt_tx, cmd_rx).run().await,
                Err(e) => {
                    let _ = evt_tx.send(Event::Error(format!("cannot open the database: {e:#}"))).await;
                }
            }
        });
    });
    (cmd_tx, evt_rx)
}

struct Bluetooth {
    _session: bluer::Session,
    adapter: Adapter,
    _agent: bluer::agent::AgentHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Brand {
    RingConn,
    Colmi,
}

struct Connected {
    device: Device,
    address: String,
    brand: Brand,
    link: Link,
    measurement: Option<Measurement>,
}

struct Measurement {
    kind: MeasureKind,
    started: Instant,
    locked: Option<Instant>,
    samples: Vec<u8>,
    last_frame: Instant,
    last_poll: Instant,
}

impl Measurement {
    fn new(kind: MeasureKind) -> Self {
        let now = Instant::now();
        Self { kind, started: now, locked: None, samples: Vec::new(), last_frame: now, last_poll: now }
    }
}

/// A Gen 2 measurement warms up for about 10 s, streams a reading every 2 s for about 20 s, then
/// stops on its own (PROTOCOL.md section 8).
pub const MEASURE_WINDOW: Duration = Duration::from_secs(20);
/// Total limit, including the wait for the ring to lock on.
const MEASURE_LIMIT: Duration = Duration::from_secs(45);
/// Silence after lock-on that means the ring has ended the session.
const MEASURE_END_SILENCE: Duration = Duration::from_secs(5);
const MEASURE_MIN_SAMPLES: usize = 5;
const LINK_CHECK: Duration = Duration::from_secs(5);
/// Run 2 of 2026-10-21 saw the ring at -98 dBm and never connected; -62 dBm connected.
const WEAK_SIGNAL: i16 = -90;

fn median(samples: &[u8]) -> f64 {
    let mut s = samples.to_vec();
    s.sort_unstable();
    let mid = s.len() / 2;
    if s.len().is_multiple_of(2) {
        (s[mid - 1] as f64 + s[mid] as f64) / 2.0
    } else {
        s[mid] as f64
    }
}

struct Backend {
    store: Store,
    events: async_channel::Sender<Event>,
    commands: async_channel::Receiver<Command>,
    bluetooth: Option<Bluetooth>,
    conn: Option<Connected>,
    last_telemetry: Option<(Instant, Vec<u8>)>,
    settings: Settings,
    rules: Vec<Rule>,
    alerts: Alerts,
    window_visible: bool,
    /// Set by Disconnect or Forget; automatic connections wait for the next Connect.
    paused: bool,
    next_attempt: Instant,
    link_checked: Instant,
    /// Last frame from the ring, and when a fetch was sent to test a quiet link.
    last_frame: Instant,
    quiet_probe: Option<Instant>,
    backoff: Duration,
    /// The sync owed for this opening of the window has run.
    synced_on_open: bool,
    /// An automatic connection found the ring unresponsive and the user has been told; cleared
    /// by the next connection.
    told_unresponsive: bool,
    last_sync_attempt: Option<i64>,
}

/// Automatic reconnection waits this long after the first failure, doubling to `MAX_BACKOFF`.
const MIN_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// A failed scheduled sync is retried no sooner than this.
const SYNC_RETRY: i64 = 10 * 60;

/// Status frames arrive with every history page; repeats are stored at most this often.
const TELEMETRY_STORE_INTERVAL: Duration = Duration::from_secs(300);
const QUIET: Duration = Duration::from_secs(4);
/// A connected ring sends a status frame every 40 to 110 s. After this long without any frame a
/// fetch is sent, and without an answer to that the link is rebuilt.
const SILENCE: Duration = Duration::from_secs(180);
const SILENCE_REPLY: Duration = Duration::from_secs(10);

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Backend {
    fn new(store: Store, events: async_channel::Sender<Event>, commands: async_channel::Receiver<Command>) -> Self {
        let settings = store.settings().unwrap_or_default();
        let rules = store.rules().unwrap_or_default();
        Self {
            store,
            events,
            commands,
            bluetooth: None,
            conn: None,
            last_telemetry: None,
            settings,
            rules,
            alerts: Alerts::default(),
            window_visible: false,
            paused: false,
            next_attempt: Instant::now(),
            link_checked: Instant::now(),
            last_frame: Instant::now(),
            quiet_probe: None,
            backoff: MIN_BACKOFF,
            synced_on_open: true,
            told_unresponsive: false,
            last_sync_attempt: None,
        }
    }

    fn alert(&mut self, stat: Stat, value: f64, taken: i64) {
        let fahrenheit = self.settings.fahrenheit;
        for alert in self.alerts.check(&self.rules, stat, value, taken, unix_now(), fahrenheit) {
            self.emit(Event::Notify(alert));
        }
    }

    fn ring_address(&self) -> Option<String> {
        match &self.conn {
            Some(c) => Some(c.address.clone()),
            None => self.store.default_ring().ok().flatten(),
        }
    }

    fn wants_connection(&self) -> bool {
        !self.paused && (self.settings.stay_connected || self.window_visible) && self.store.ring_to_connect().ok().flatten().is_some()
    }

    /// Runs every 30 s. Uses wall-clock time, so a schedule that came due while the phone was
    /// suspended runs at the first check after it wakes.
    async fn maintain(&mut self) {
        let now = unix_now();
        self.check_link().await;
        if self.conn.is_none() && self.wants_connection() && Instant::now() >= self.next_attempt {
            match self.connect().await {
                Ok(()) => self.backoff = MIN_BACKOFF,
                Err(e) => {
                    tracing::info!(error = %format!("{e:#}"), "automatic connection failed");
                    // Retrying cannot fix this one, so it is shown once rather than only logged.
                    if e.is::<Unresponsive>() && !self.told_unresponsive {
                        self.told_unresponsive = true;
                        self.emit(Event::Error(format!("{e:#}")));
                    }
                    self.emit(Event::Link(LinkState::Disconnected));
                    self.next_attempt = Instant::now() + self.backoff;
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        if self.conn.is_some() && !self.wants_connection() && self.conn.as_ref().is_some_and(|c| c.measurement.is_none()) {
            self.disconnect().await;
        }

        let Some(ring) = self.ring_address() else { return };
        if self.conn.is_none() && self.store.is_forgotten(&ring).unwrap_or(true) {
            return;
        }
        let last = self.store.last_sync(&ring).ok().flatten();
        if let Some(last) = last {
            self.alert(Stat::DaysSinceSync, (now - last) as f64 / 86_400.0, now);
        }
        let due = self.settings.sync_interval_minutes.is_some_and(|m| last.is_none_or(|l| now - l >= m as i64 * 60));
        let retry_ok = self.last_sync_attempt.is_none_or(|t| now - t >= SYNC_RETRY);
        if due && retry_ok && !self.paused {
            self.scheduled_sync().await;
        }
    }

    /// Syncs over the open connection, or connects just for the sync when no connection is kept.
    async fn scheduled_sync(&mut self) {
        self.last_sync_attempt = Some(unix_now());
        let temporary = self.conn.is_none();
        if temporary && self.connect().await.is_err() {
            self.emit(Event::Link(LinkState::Disconnected));
            return;
        }
        if let Err(e) = self.sync().await {
            tracing::info!(error = %format!("{e:#}"), "scheduled sync failed");
        }
        if temporary && !self.wants_connection() {
            self.disconnect().await;
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    async fn run(mut self) {
        self.refresh();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut maintenance = tokio::time::interval(Duration::from_secs(30));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Ok(command) = command else { break };
                    if matches!(command, Command::Shutdown) {
                        self.disconnect().await;
                        break;
                    }
                    if let Err(e) = self.handle(command).await {
                        self.emit(Event::Error(format!("{e:#}")));
                    }
                }
                received = next_frame(&mut self.conn) => match received {
                    Received::Frame(f) => {
                        self.last_frame = Instant::now();
                        self.quiet_probe = None;
                        self.on_frame(&f);
                    }
                    Received::Data(_) => {
                        self.last_frame = Instant::now();
                        self.quiet_probe = None;
                    }
                    Received::Closed => self.lost_connection(),
                    Received::Quiet => {}
                },
                _ = tick.tick() => {
                    self.check_link().await;
                    self.measure_tick().await;
                }
                _ = maintenance.tick() => self.maintain().await,
            }
        }
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Connect if self.conn.is_none() => {
                self.paused = false;
                self.backoff = MIN_BACKOFF;
                let result = self.connect().await;
                if result.is_err() {
                    self.emit(Event::Link(LinkState::Disconnected));
                }
                result
            }
            Command::Connect => {
                // The window can be showing a link that BlueZ has already dropped.
                if self.link_alive().await {
                    let name = self.store.ring(&self.ring_address().unwrap_or_default()).ok().flatten().map(|r| r.display_name());
                    self.emit(Event::Link(LinkState::Connected { name: name.unwrap_or_else(|| "RingConn".into()) }));
                    return Ok(());
                }
                self.conn = None;
                self.paused = false;
                self.backoff = MIN_BACKOFF;
                let result = self.connect().await;
                if result.is_err() {
                    self.emit(Event::Link(LinkState::Disconnected));
                }
                result
            }
            Command::Disconnect => {
                self.paused = true;
                self.disconnect().await;
                Ok(())
            }
            Command::WindowVisible(visible) => {
                self.window_visible = visible;
                if visible {
                    self.synced_on_open = !self.settings.sync_on_open;
                    self.next_attempt = Instant::now();
                    self.sync_on_open().await;
                    self.refresh();
                }
                Ok(())
            }
            Command::ReloadSettings => {
                self.settings = self.store.settings()?;
                let rules = self.store.rules()?;
                for rule in &rules {
                    if !self.rules.contains(rule) {
                        self.alerts.forget(rule.id);
                    }
                }
                self.rules = rules;
                self.next_attempt = Instant::now();
                self.refresh();
                Ok(())
            }
            Command::ForgetRing => self.forget_ring().await,
            Command::ConnectTo(address) => {
                let address: Address = address.parse().context("bad ring address")?;
                if self.conn.as_ref().is_some_and(|c| c.address == address.to_string()) {
                    return Ok(());
                }
                self.disconnect().await;
                self.paused = false;
                self.backoff = MIN_BACKOFF;
                let result = self.connect_to(address).await;
                if result.is_err() {
                    self.emit(Event::Link(LinkState::Disconnected));
                }
                result
            }
            Command::AddRing => self.add_ring().await,
            Command::SwitchRing(address) => {
                if self.ring_address().as_deref() == Some(address.as_str()) {
                    return Ok(());
                }
                if self.conn.as_ref().is_some_and(|c| c.measurement.is_some()) {
                    self.finish_measure(false).await?;
                }
                self.disconnect().await;
                self.store.set_default_ring(&address)?;
                self.alerts = Alerts::default();
                self.paused = false;
                self.backoff = MIN_BACKOFF;
                self.next_attempt = Instant::now();
                self.last_sync_attempt = None;
                self.refresh();
                self.maintain().await;
                Ok(())
            }
            Command::Sync => {
                self.ensure_link().await?;
                self.sync().await
            }
            Command::Measure(kind) => {
                self.ensure_link().await?;
                self.start_measure(kind).await
            }
            Command::StopMeasure => self.finish_measure(false).await,
            Command::Refresh => {
                self.refresh();
                Ok(())
            }
            Command::Shutdown => Ok(()),
        }
    }

    fn refresh(&self) {
        let summary = self.ring_address().and_then(|r| self.store.summary(&r).ok()).unwrap_or_default();
        self.emit(Event::Summary(Box::new(summary)));
    }

    async fn sync_on_open(&mut self) {
        if self.synced_on_open || self.conn.is_none() {
            return;
        }
        self.synced_on_open = true;
        self.last_sync_attempt = Some(unix_now());
        if let Err(e) = self.sync().await {
            self.emit(Event::Error(format!("{e:#}")));
        }
    }

    async fn forget_ring(&mut self) -> Result<()> {
        let ring = self.ring_address();
        self.paused = true;
        self.disconnect().await;
        if let Some(ring) = &ring {
            self.store.forget_ring(ring)?;
        }
        if let Some(addr) = ring.and_then(|a| a.parse::<Address>().ok()) {
            let adapter = self.bluetooth().await?.adapter.clone();
            if let Err(e) = adapter.remove_device(addr).await {
                tracing::info!(error = %e, "removing the bond failed");
            }
        }
        self.emit(Event::Error("ring forgotten; its history stays here. Connect pairs a ring again".into()));
        self.refresh();
        Ok(())
    }

    /// Rings stored here and not forgotten; a search for a new ring skips them.
    fn kept_rings(&self) -> Vec<Address> {
        self.store.rings().unwrap_or_default().into_iter().filter(|r| !r.forgotten).filter_map(|r| r.info.address.parse().ok()).collect()
    }

    fn choices(&self, found: &[(Address, String, i16)]) -> Vec<RingChoice> {
        found
            .iter()
            .map(|(a, n, r)| RingChoice {
                address: a.to_string(),
                name: self.store.ring(&a.to_string()).ok().flatten().map_or_else(|| n.clone(), |i| i.display_name()),
                rssi: *r,
            })
            .collect()
    }

    /// Offers every new ring advertising, even a single one, since it is about to be paired.
    async fn add_ring(&mut self) -> Result<()> {
        self.emit(Event::Error("looking for new rings for 10 seconds".into()));
        let skip = self.kept_rings();
        let adapter = self.bluetooth().await?.adapter.clone();
        let found = transport::scan_all(&adapter, Duration::from_secs(10), &skip).await?;
        if found.is_empty() {
            bail!("no new ring is advertising; seat it in its case for a moment, take it out and try again");
        }
        self.emit(Event::ChooseRing(self.choices(&found)));
        Ok(())
    }

    async fn bluetooth(&mut self) -> Result<&Bluetooth> {
        if self.bluetooth.is_none() {
            let session = bluer::Session::new().await.context("cannot reach the Bluetooth service")?;
            let adapter = session.default_adapter().await.context("no Bluetooth adapter")?;
            adapter.set_powered(true).await.context("cannot turn Bluetooth on")?;
            let agent = session.register_agent(transport::agent()).await.context("cannot register the pairing agent")?;
            self.bluetooth = Some(Bluetooth { _session: session, adapter, _agent: agent });
        }
        Ok(self.bluetooth.as_ref().unwrap())
    }

    /// The default ring, or a ring bonded outside Ringmaster when none is stored, or else the only
    /// new ring advertising. `None` when several new rings are advertising and the user has been
    /// asked to choose.
    async fn find_ring(&mut self) -> Result<Option<Address>> {
        let remembered = self.store.ring_to_connect()?.and_then(|a| a.parse::<Address>().ok());
        let adapter = self.bluetooth().await?.adapter.clone();
        let target = match remembered {
            Some(a) => Some(a),
            None if self.store.rings()?.is_empty() => transport::known_rings(&adapter).await?.first().copied(),
            None => None,
        };
        if let Some(addr) = target {
            if adapter.device(addr)?.is_connected().await.unwrap_or(false) {
                return Ok(Some(addr));
            }
        }
        self.emit(Event::Link(LinkState::Searching));
        let not_advertising = || anyhow!("the ring is not advertising; seat it in its case for a moment, take it out and try again");
        let (addr, rssi) = match target {
            Some(want) => transport::scan(&adapter, Duration::from_secs(20), Some(want)).await?.ok_or_else(not_advertising)?,
            // Taking the first ring heard could pair a stranger's ring.
            None => match transport::scan_all(&adapter, Duration::from_secs(10), &self.kept_rings()).await?.as_slice() {
                [] => return Err(not_advertising()),
                [(addr, _, rssi)] => (*addr, *rssi),
                several => {
                    self.emit(Event::ChooseRing(self.choices(several)));
                    return Ok(None);
                }
            },
        };
        if rssi < WEAK_SIGNAL {
            self.emit(Event::Error(format!("weak signal ({rssi} dBm); keep the ring close to the phone")));
        }
        Ok(Some(addr))
    }

    async fn connect(&mut self) -> Result<()> {
        self.emit(Event::Link(LinkState::Searching));
        match self.find_ring().await? {
            Some(address) => self.connect_to(address).await,
            None => {
                self.emit(Event::Link(LinkState::Disconnected));
                Ok(())
            }
        }
    }

    async fn connect_to(&mut self, address: Address) -> Result<()> {
        let device = self.bluetooth().await?.adapter.device(address)?;
        let brand = if transport::is_colmi(&device).await { Brand::Colmi } else { Brand::RingConn };

        self.emit(Event::Link(LinkState::Connecting));
        let _ = device.set_trusted(true).await;
        transport::connect(&device).await?;
        // Colmi rings neither bond nor authenticate.
        if brand == Brand::RingConn && !device.is_paired().await.unwrap_or(false) {
            self.emit(Event::Link(LinkState::Pairing));
            transport::pair(&device).await?;
        }
        transport::wait_ready(&device).await?;

        let fallback = match brand {
            Brand::RingConn => "RingConn",
            Brand::Colmi => "Colmi",
        };
        let name = device.name().await.ok().flatten().unwrap_or_else(|| fallback.into());
        let (serial, firmware, hardware) = transport::device_information(&device).await;
        let stored_brand = match brand {
            Brand::RingConn => crate::store::RINGCONN,
            Brand::Colmi => crate::store::COLMI,
        };
        self.store.upsert_ring(&RingInfo {
            address: address.to_string(),
            name: Some(name),
            serial,
            firmware,
            hardware,
            nickname: None,
            brand: Some(stored_brand.into()),
        })?;
        self.store.set_default_ring(&address.to_string())?;
        let name = self.store.ring(&address.to_string())?.map(|r| r.display_name()).unwrap_or_else(|| fallback.into());

        let (device, link) = match brand {
            Brand::RingConn => {
                self.emit(Event::Link(LinkState::Authenticating));
                self.open_ringconn(device, address).await?
            }
            Brand::Colmi => {
                let mut link = Link::open_colmi(&device).await?;
                colmi_ring::greet(&mut link).await?;
                (device, link)
            }
        };

        self.conn = Some(Connected { device, address: address.to_string(), brand, link, measurement: None });
        self.told_unresponsive = false;
        self.last_frame = Instant::now();
        self.quiet_probe = None;
        self.emit(Event::Link(LinkState::Connected { name }));
        self.refresh();
        if self.window_visible {
            self.sync_on_open().await;
        }
        Ok(())
    }

    async fn open_ringconn(&mut self, device: Device, address: Address) -> Result<(Device, Link)> {
        let mut link = Link::open(&device).await?;
        let (device, mut link) = match authenticate(&mut link, address.0).await {
            Ok(()) => (device, link),
            Err(e) if e.is::<NoAnswer>() => {
                // Starts over as a fresh launch would: a new Bluetooth session and a new link.
                tracing::info!("no answer from the ring; starting a new Bluetooth session and link");
                drop(link);
                let _ = tokio::time::timeout(Duration::from_secs(5), device.disconnect()).await;
                self.bluetooth = None;
                tokio::time::sleep(Duration::from_secs(2)).await;
                let device = self.bluetooth().await?.adapter.device(address)?;
                transport::connect(&device).await?;
                transport::wait_ready(&device).await?;
                let mut link = Link::open(&device).await?;
                if let Err(e) = authenticate(&mut link, address.0).await {
                    if !e.is::<NoAnswer>() {
                        return Err(e);
                    }
                    drop(link);
                    let _ = tokio::time::timeout(Duration::from_secs(5), device.disconnect()).await;
                    return Err(Unresponsive.into());
                }
                (device, link)
            }
            Err(e) => return Err(e),
        };
        link.send(&cmd::FETCH).await?;
        Ok((device, link))
    }

    async fn disconnect(&mut self) {
        if let Some(mut c) = self.conn.take() {
            if let Some(m) = &c.measurement {
                let _ = c.link.send(&live_off(c.brand, m.kind)).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), c.device.disconnect()).await;
        }
        self.emit(Event::Link(LinkState::Disconnected));
    }

    /// BlueZ can drop the link without ending the notify stream, which leaves `conn` set until
    /// this check notices.
    async fn check_link(&mut self) {
        if self.conn.is_none() || self.link_checked.elapsed() < LINK_CHECK {
            return;
        }
        self.link_checked = Instant::now();
        if !self.link_alive().await {
            tracing::info!("BlueZ reports the ring disconnected without closing the notify stream");
            self.lost_connection();
            return;
        }
        // BlueZ can also reconnect a trusted ring by itself after a drop; the link then looks
        // connected while notifications are off and the ring is not authenticated.
        if self.last_frame.elapsed() < SILENCE || self.conn.as_ref().is_some_and(|c| c.measurement.is_some()) {
            return;
        }
        match self.quiet_probe {
            None => {
                self.quiet_probe = Some(Instant::now());
                if let Some(c) = self.conn.as_mut() {
                    // Colmi rings send nothing unprompted, so this also keeps their battery current.
                    let probe = match c.brand {
                        Brand::RingConn => cmd::FETCH.to_vec(),
                        Brand::Colmi => colmi::request::battery().to_vec(),
                    };
                    let _ = c.link.send(&probe).await;
                }
            }
            Some(sent) if sent.elapsed() >= SILENCE_REPLY => {
                tracing::info!("no frame from the ring for {} s; rebuilding the link", self.last_frame.elapsed().as_secs());
                self.quiet_probe = None;
                if let Some(c) = self.conn.take() {
                    let _ = tokio::time::timeout(Duration::from_secs(5), c.device.disconnect()).await;
                }
                self.lost_connection();
            }
            Some(_) => {}
        }
    }

    /// Reconnects first when the link has dropped, so a request made just after a silent drop
    /// still goes through.
    async fn ensure_link(&mut self) -> Result<()> {
        if self.conn.is_some() && !self.link_alive().await {
            self.lost_connection();
        }
        if self.conn.is_none() {
            self.paused = false;
            if let Err(e) = self.connect().await {
                self.emit(Event::Link(LinkState::Disconnected));
                return Err(e);
            }
        }
        Ok(())
    }

    async fn link_alive(&self) -> bool {
        match &self.conn {
            Some(c) => c.device.is_connected().await.unwrap_or(false),
            None => false,
        }
    }

    fn lost_connection(&mut self) {
        self.conn = None;
        self.emit(Event::Link(LinkState::Disconnected));
        if self.wants_connection() {
            self.next_attempt = Instant::now() + Duration::from_secs(10);
        } else {
            self.emit(Event::Error("the ring disconnected".into()));
        }
    }

    fn on_frame(&mut self, frame: &[u8]) {
        if self.conn.as_ref().is_some_and(|c| c.brand == Brand::Colmi) {
            self.on_colmi_frame(frame);
            return;
        }
        match frame.first() {
            Some(&op::DESCRIPTOR_QUERY | &op::DESCRIPTOR_FETCH) => self.on_descriptor(frame),
            Some(&op::LIVE) => {
                let Some(m) = self.conn.as_mut().and_then(|c| c.measurement.as_mut()) else { return };
                m.last_frame = Instant::now();
                let value = match m.kind {
                    MeasureKind::HeartRate => live_heart_rate(frame),
                    MeasureKind::Spo2 => live_spo2(frame),
                };
                if let Some(v) = value {
                    m.locked.get_or_insert_with(Instant::now);
                    m.samples.push(v);
                }
                let progress = m.locked.map_or(0.0, |t| (t.elapsed().as_secs_f64() / MEASURE_WINDOW.as_secs_f64()).min(1.0));
                self.emit(Event::MeasureProgress { value, progress });
            }
            _ => {}
        }
    }

    fn on_descriptor(&mut self, frame: &[u8]) {
        let (Some(d), Some(c)) = (Descriptor::parse(frame), self.conn.as_ref()) else { return };
        let changed = self.last_telemetry.as_ref().map(|(_, raw)| raw.as_slice() != frame).unwrap_or(true);
        let due = self.last_telemetry.as_ref().map(|(t, _)| t.elapsed() >= TELEMETRY_STORE_INTERVAL).unwrap_or(true);
        if changed || due {
            if let Err(e) = self.store.store_telemetry(&c.address, unix_now(), &d, frame) {
                tracing::warn!(error = %e, "storing telemetry failed");
            }
            self.last_telemetry = Some((Instant::now(), frame.to_vec()));
            if let Ok((steps, _)) = self.store.steps_today(&c.address) {
                let left = (self.settings.step_goal as i64 - steps).max(0);
                self.alert(Stat::DailySteps, steps as f64, unix_now());
                self.alert(Stat::StepsToGoal, left as f64, unix_now());
            }
        }
        if changed {
            self.emit(Event::Telemetry(d.clone()));
        }
        let now = unix_now();
        self.alert(Stat::Battery, d.battery as f64, now);
        self.alert(Stat::SkinTemp, d.skin_temp[0].max(d.skin_temp[1]) as f64, now);
        if let Some((percent, _)) = d.case_battery() {
            self.alert(Stat::CaseBattery, percent as f64, now);
        }
    }

    async fn sync(&mut self) -> Result<()> {
        if self.conn.is_none() {
            bail!("connect to the ring first");
        }
        if self.conn.as_ref().is_some_and(|c| c.measurement.is_some()) {
            self.finish_measure(false).await?;
        }
        if self.conn.as_ref().is_some_and(|c| c.brand == Brand::Colmi) {
            return self.sync_colmi().await;
        }
        self.emit(Event::SyncStarted);
        // Pages arrive outside the main loop, so the quiet-link check would otherwise count a
        // long sync as silence.
        self.last_frame = Instant::now();
        let mut new_records = 0;
        let mut error = None;
        for channel in Channel::ALL {
            match self.drain_channel(channel).await {
                Ok(n) => new_records += n,
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        tracing::info!(new_records, error = error.as_deref().unwrap_or(""), "sync finished");
        if error.is_none() {
            if let Some(ring) = self.ring_address() {
                self.store.mark_synced(&ring, unix_now())?;
            }
        }
        self.emit(Event::SyncFinished { error });
        self.refresh();
        Ok(())
    }

    async fn drain_channel(&mut self, channel: Channel) -> Result<usize> {
        let started = unix_now();
        let (mut drain, actions) = ChannelDrain::start(channel, started);
        let mut new_records = 0;
        let mut outcome = self.execute(&mut drain, actions, &mut new_records).await?;
        while outcome.is_none() {
            let received = {
                let c = self.conn.as_mut().context("the ring disconnected")?;
                tokio::select! {
                    r = c.link.recv(QUIET) => r,
                    command = self.commands.recv() => {
                        match command {
                            Ok(Command::Shutdown) | Ok(Command::Disconnect) | Err(_) => {
                                let _ = self.events.try_send(Event::Error("sync interrupted".into()));
                                bail!("sync interrupted");
                            }
                            _ => continue,
                        }
                    }
                }
            };
            let actions = match received {
                Received::Frame(f) => {
                    self.on_frame(&f);
                    drain.on_frame(&f)
                }
                Received::Quiet => drain.on_quiet(),
                Received::Data(_) => continue,
                Received::Closed => {
                    self.lost_connection();
                    bail!("the ring disconnected during sync");
                }
            };
            outcome = self.execute(&mut drain, actions, &mut new_records).await?;
        }
        let outcome = outcome.unwrap();
        let address = self.conn.as_ref().map(|c| c.address.clone()).unwrap_or_default();
        let record = SyncRecord {
            started,
            finished: unix_now(),
            channel: channel.id(),
            outcome: format!("{outcome:?}"),
            pages: drain.pages,
            records: drain.records,
        };
        self.store.record_sync(&address, &record)?;
        if let Outcome::Failed(reason) = outcome {
            bail!("channel {:02x}: {reason}", channel.id());
        }
        Ok(new_records)
    }

    /// A failed store returns before the following ACK is sent, so the ring keeps that page.
    async fn execute(&mut self, drain: &mut ChannelDrain, actions: Vec<Action>, new_records: &mut usize) -> Result<Option<Outcome>> {
        let mut finished = None;
        for action in actions {
            match action {
                Action::Store(batch) => {
                    let address = self.conn.as_ref().map(|c| c.address.clone()).context("the ring disconnected")?;
                    let added =
                        self.store.store_batch(&address, drain.channel().id(), unix_now(), &batch).context("saving history failed")?;
                    *new_records += added;
                    if let (Batch::Epochs(epochs), true) = (&batch, added > 0) {
                        for e in epochs {
                            let taken = ringconn::time::to_unix(e.counter);
                            let values = [
                                (Stat::HeartRate, e.heart_rate().map(f64::from)),
                                (Stat::Hrv, e.hrv().map(f64::from)),
                                (Stat::Spo2, e.spo2().map(f64::from)),
                                (Stat::RespRate, e.respiratory_rate().map(f64::from)),
                            ];
                            for (stat, value) in values {
                                if let Some(v) = value {
                                    self.alert(stat, v, taken);
                                }
                            }
                        }
                    }
                }
                Action::Send { bytes, after } => {
                    tokio::time::sleep(after).await;
                    self.conn.as_mut().context("the ring disconnected")?.link.send(&bytes).await?;
                }
                Action::Progress { remaining } => {
                    self.emit(Event::SyncProgress { records: drain.records, remaining });
                }
                Action::Finished(outcome) => finished = Some(outcome),
            }
        }
        Ok(finished)
    }

    async fn start_measure(&mut self, kind: MeasureKind) -> Result<()> {
        let c = self.conn.as_mut().context("connect to the ring first")?;
        if c.measurement.is_some() {
            return Ok(());
        }
        if c.brand == Brand::Colmi {
            c.link.send(&colmi::live::start(colmi_kind(kind))).await?;
            c.measurement = Some(Measurement::new(kind));
            self.emit(Event::MeasureStarted(kind));
            return Ok(());
        }
        // ringlink's sequence. Every sequence tried works on Gen 2; ringlink reports that Gen 3 needs
        // the status query before it accepts the mode.
        c.link.send(&cmd::STATUS_QUERY).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        c.link
            .send(match kind {
                MeasureKind::HeartRate => &cmd::LIVE_HR,
                MeasureKind::Spo2 => &cmd::LIVE_SPO2,
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        c.measurement = Some(Measurement::new(kind));
        self.emit(Event::MeasureStarted(kind));
        Ok(())
    }

    /// Ends the measurement; with `keep`, saves the median if enough readings arrived.
    async fn finish_measure(&mut self, keep: bool) -> Result<()> {
        let Some(c) = self.conn.as_mut() else { return Ok(()) };
        let Some(m) = c.measurement.take() else { return Ok(()) };
        let off = c.link.send(&live_off(c.brand, m.kind)).await;
        let result = (keep && m.samples.len() >= MEASURE_MIN_SAMPLES).then(|| Spot {
            value: median(&m.samples),
            samples: m.samples.len() as i64,
            unix: unix_now(),
            range: m.samples.iter().min().zip(m.samples.iter().max()).map(|(l, h)| (*l as f64, *h as f64)),
        });
        if let Some(spot) = &result {
            let address = c.address.clone();
            self.store.store_spot(&address, m.kind, spot)?;
        }
        self.emit(Event::MeasureFinished { kind: m.kind, result });
        self.refresh();
        off
    }

    /// Gen 2 streams readings unprompted and ignores polls; ringlink reports that Gen 3 sends
    /// them only when polled, so a poll goes out every 2 s.
    async fn measure_tick(&mut self) {
        let Some(c) = self.conn.as_mut() else { return };
        let Some(m) = c.measurement.as_mut() else { return };
        let ended = m.locked.is_some_and(|t| t.elapsed() >= MEASURE_WINDOW)
            || (m.locked.is_some() && m.last_frame.elapsed() >= MEASURE_END_SILENCE)
            || m.started.elapsed() >= MEASURE_LIMIT;
        if ended {
            if let Err(e) = self.finish_measure(true).await {
                self.emit(Event::Error(format!("{e:#}")));
            }
            return;
        }
        match c.brand {
            Brand::RingConn if m.last_poll.elapsed() >= Duration::from_secs(2) => {
                m.last_poll = Instant::now();
                let _ = c.link.send(&cmd::POLL).await;
            }
            Brand::Colmi if m.last_poll.elapsed() >= colmi_ring::KEEP_GOING => {
                m.last_poll = Instant::now();
                let _ = c.link.send(&colmi::live::keep_going(colmi_kind(m.kind))).await;
            }
            _ => {}
        }
    }
}

fn colmi_kind(kind: MeasureKind) -> colmi::live::Kind {
    match kind {
        MeasureKind::HeartRate => colmi::live::Kind::HeartRate,
        MeasureKind::Spo2 => colmi::live::Kind::Spo2,
    }
}

fn live_off(brand: Brand, kind: MeasureKind) -> Vec<u8> {
    match brand {
        Brand::RingConn => cmd::LIVE_OFF.to_vec(),
        Brand::Colmi => colmi::live::stop(colmi_kind(kind)).to_vec(),
    }
}

async fn next_frame(conn: &mut Option<Connected>) -> Received {
    match conn {
        Some(c) => c.link.recv(Duration::from_secs(3600)).await,
        None => std::future::pending().await,
    }
}

/// No reply at all to the status request, as when notifications are not enabled on the ring.
#[derive(Debug)]
struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the ring did not answer")
    }
}

impl std::error::Error for NoAnswer {}

/// No answer even over a new Bluetooth session. Chained PPG sessions left a ring like this on
/// 2026-10-27 and 28 until it was put in its case.
#[derive(Debug)]
struct Unresponsive;

impl std::fmt::Display for Unresponsive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the ring connects but is not responding; seat it in its case for a moment and take it out to restart it")
    }
}

impl std::error::Error for Unresponsive {}

async fn authenticate(link: &mut Link, mac: [u8; 6]) -> Result<()> {
    link.send(&cmd::STATUS).await?;
    let status = link.wait_for(op::STATUS, Duration::from_secs(5), |_| {}).await.ok_or(NoAnswer)?;
    let reply =
        ringconn::auth::reply_to(&status, mac).with_context(|| format!("unexpected status reply {}", ringconn::frame::hex(&status)))?;
    link.send(&reply).await?;
    link.wait_for(op::STATUS, Duration::from_secs(5), |_| {}).await.context("the ring did not accept authentication")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn median_of_readings() {
        assert_eq!(super::median(&[70, 62, 64]), 64.0);
        assert_eq!(super::median(&[62, 64, 70, 61]), 63.0);
    }
}
