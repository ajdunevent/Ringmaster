//! The one-line status shown beside Ringmaster in the desktop's list of apps running in the
//! background, sent through the Background portal while no window is open.

use crate::backend::{Event, LinkState};
use chrono::{DateTime, Local, TimeZone};

/// The portal takes messages shorter than 96 characters.
const MAX_CHARS: usize = 95;

/// How long the status stays unchanged before it is sent.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct Status {
    /// `None` when no ring is set up.
    ring: Option<String>,
    link: Option<LinkState>,
    battery: Option<u8>,
    charging: bool,
    syncing: bool,
    last_sync: Option<i64>,
    sync_stopped: bool,
}

impl Status {
    /// Takes in an event; returns whether the text may have changed.
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::Summary(s) => {
                self.ring = s.ring.as_ref().map(|r| r.display_name());
                self.last_sync = s.last_sync;
            }
            Event::Link(state) => {
                if !matches!(state, LinkState::Connected { .. }) {
                    self.battery = None;
                    self.charging = false;
                    self.syncing = false;
                }
                self.link = Some(state.clone());
            }
            Event::Telemetry(d) => {
                self.battery = Some(d.battery);
                self.charging = d.ring_state() == ringconn::records::RingState::Charging;
            }
            Event::Battery { percent, charging } => {
                self.battery = *percent;
                self.charging = *charging;
            }
            Event::SyncStarted => self.syncing = true,
            Event::SyncFinished { error } => {
                self.syncing = false;
                self.sync_stopped = error.is_some();
            }
            _ => return false,
        }
        true
    }

    pub fn text(&self, now: DateTime<Local>) -> String {
        let Some(ring) = &self.ring else { return "No rings configured".into() };
        let connected = matches!(self.link, Some(LinkState::Connected { .. }));
        let mut parts = vec![match &self.link {
            _ if self.syncing => format!("Syncing {ring}…"),
            Some(LinkState::Connected { .. }) => format!("Connected to {ring}"),
            Some(LinkState::Searching) => "Searching…".into(),
            Some(LinkState::Connecting) => "Connecting…".into(),
            Some(LinkState::Pairing) => "Pairing…".into(),
            Some(LinkState::Authenticating) => "Authenticating…".into(),
            Some(LinkState::Disconnected) | None => "Not connected".into(),
        }];
        if connected {
            match (self.battery, self.charging) {
                (Some(p), true) => parts.push(format!("{p} %, charging")),
                (Some(p), false) => parts.push(format!("{p} %")),
                (None, true) => parts.push("charging".into()),
                (None, false) => {}
            }
        }
        if !self.syncing {
            if self.sync_stopped {
                parts.push("sync stopped".into());
            } else if let Some(t) = self.last_sync.and_then(|u| Local.timestamp_opt(u, 0).single()) {
                parts.push(synced(t, now));
            }
        }
        shorten(parts.join(" · "))
    }
}

/// "synced 14:32", "synced yesterday 23:10", "synced 3 Nov 08:15".
fn synced(t: DateTime<Local>, now: DateTime<Local>) -> String {
    match (now.date_naive() - t.date_naive()).num_days() {
        0 => t.format("synced %H:%M").to_string(),
        1 => t.format("synced yesterday %H:%M").to_string(),
        _ => t.format("synced %-d %b %H:%M").to_string(),
    }
}

fn shorten(text: String) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(MAX_CHARS - 1).collect();
    short.push('…');
    short
}

/// Sends the status to the portal once it has settled, and nothing when it has not changed.
/// Only a Flatpak has a status there; elsewhere this does nothing.
pub struct Portal {
    shown: String,
    queue: Option<async_channel::Sender<String>>,
}

impl Portal {
    pub fn new() -> Self {
        if !crate::autostart::in_flatpak() {
            return Self { shown: String::new(), queue: None };
        }
        let (tx, rx) = async_channel::unbounded::<String>();
        glib::MainContext::default().spawn_local(async move {
            let proxy = match ashpd::desktop::background::BackgroundProxy::new().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(error = %e, "the Background portal is unavailable");
                    return;
                }
            };
            if proxy.version() < 2 {
                tracing::debug!("the Background portal has no status messages");
                return;
            }
            let mut sent = String::new();
            while let Ok(mut message) = rx.recv().await {
                // A failed connection passes through several states within moments; only the
                // state it settles in is sent.
                glib::timeout_future(SETTLE).await;
                while let Ok(newer) = rx.try_recv() {
                    message = newer;
                }
                if message == sent {
                    continue;
                }
                sent = message.clone();
                let options = ashpd::desktop::background::SetStatusOptions::default().set_message(&message);
                if let Err(e) = proxy.set_status(options).await {
                    tracing::debug!(error = %e, "cannot set the background status");
                }
            }
        });
        Self { shown: String::new(), queue: Some(tx) }
    }

    pub fn show(&mut self, text: String) {
        let Some(queue) = &self.queue else { return };
        if text != self.shown {
            let _ = queue.try_send(text.clone());
            self.shown = text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RingInfo, Summary};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    fn with_ring(last_sync: Option<i64>) -> Status {
        let mut s = Status::default();
        let ring = RingInfo { address: "AA".into(), name: Some("Gen2".into()), ..Default::default() };
        s.apply(&Event::Summary(Box::new(Summary { ring: Some(ring), last_sync, ..Default::default() })));
        s
    }

    #[test]
    fn no_ring() {
        assert_eq!(Status::default().text(at(2026, 11, 25, 12, 0)), "No rings configured");
    }

    #[test]
    fn connected_with_battery_and_sync() {
        let now = at(2026, 11, 25, 16, 0);
        let mut s = with_ring(Some(at(2026, 11, 25, 15, 40).timestamp()));
        s.apply(&Event::Link(LinkState::Connected { name: "Gen2".into() }));
        s.apply(&Event::Battery { percent: Some(72), charging: false });
        assert_eq!(s.text(now), "Connected to Gen2 · 72 % · synced 15:40");
        s.apply(&Event::SyncStarted);
        assert_eq!(s.text(now), "Syncing Gen2… · 72 %");
        s.apply(&Event::SyncFinished { error: Some("lost".into()) });
        assert_eq!(s.text(now), "Connected to Gen2 · 72 % · sync stopped");
    }

    #[test]
    fn disconnected_drops_battery() {
        let now = at(2026, 11, 25, 9, 0);
        let mut s = with_ring(Some(at(2026, 11, 24, 23, 10).timestamp()));
        s.apply(&Event::Link(LinkState::Connected { name: "Gen2".into() }));
        s.apply(&Event::Battery { percent: None, charging: true });
        assert_eq!(s.text(now), "Connected to Gen2 · charging · synced yesterday 23:10");
        s.apply(&Event::Link(LinkState::Disconnected));
        assert_eq!(s.text(now), "Not connected · synced yesterday 23:10");
    }

    #[test]
    fn long_names_fit() {
        let mut s = with_ring(None);
        s.ring = Some("x".repeat(200));
        s.apply(&Event::Link(LinkState::Connected { name: String::new() }));
        let text = s.text(at(2026, 11, 25, 9, 0));
        assert_eq!(text.chars().count(), MAX_CHARS);
        assert!(text.ends_with('…'));
    }
}
