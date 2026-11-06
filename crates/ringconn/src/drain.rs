//! History drain for one channel (section 7.4), as a state machine: frames in, actions out.

use crate::frame::{checksum_ok, cmd, op};
use crate::records::{Epoch, OpticalRecord, Page, SportRecord};
use crate::{time, Channel};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Batch {
    Epochs(Vec<Epoch>),
    Sport(Vec<SportRecord>),
    Optical(Vec<OpticalRecord>),
    /// Raw `0x50` frame.
    EventLog(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Has to be durably stored before any later `Send` is written: the ACK that follows lets the
    /// ring delete the page.
    Store(Batch),
    Send {
        bytes: Vec<u8>,
        after: Duration,
    },
    /// Records the ring reports as still to come on this channel.
    Progress {
        remaining: u16,
    },
    Finished(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Ended with `0x50`.
    Complete,
    /// The sync-open reply said nothing is pending.
    Empty,
    /// Fetches were answered by descriptors alone.
    NoMoreData,
    /// The ring stopped sending.
    Quiet,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Opening,
    Streaming,
    Done,
}

/// Consecutive fetches without a page before the channel counts as drained.
const MAX_IDLE_FETCHES: u8 = 3;
const MAX_QUIET: u8 = 2;

#[derive(Debug)]
pub struct ChannelDrain {
    channel: Channel,
    phase: Phase,
    page_since_fetch: bool,
    idle_fetches: u8,
    quiet: u8,
    pub pages: usize,
    pub records: usize,
}

fn send(bytes: &[u8]) -> Action {
    Action::Send { bytes: bytes.to_vec(), after: Duration::ZERO }
}

impl ChannelDrain {
    pub fn start(channel: Channel, unix_now: i64) -> (Self, Vec<Action>) {
        let drain = Self { channel, phase: Phase::Opening, page_since_fetch: false, idle_fetches: 0, quiet: 0, pages: 0, records: 0 };
        (drain, vec![send(&cmd::sync_open(time::cursor(unix_now), channel.id()))])
    }

    pub fn channel(&self) -> Channel {
        self.channel
    }

    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    fn finish(&mut self, outcome: Outcome) -> Vec<Action> {
        self.phase = Phase::Done;
        vec![Action::Finished(outcome)]
    }

    pub fn on_frame(&mut self, frame: &[u8]) -> Vec<Action> {
        let Some(&code) = frame.first() else { return Vec::new() };
        self.quiet = 0;
        match self.phase {
            Phase::Done => Vec::new(),
            Phase::Opening if code == op::SYNC_OPEN => {
                if frame.get(1) == Some(&0xff) {
                    return self.finish(Outcome::Empty);
                }
                self.phase = Phase::Streaming;
                vec![send(&cmd::FETCH)]
            }
            Phase::Opening => Vec::new(),
            Phase::Streaming => match code {
                op::EPOCH_PAGE | op::OPTICAL_PAGE | op::SPORT_PAGE => self.on_page(frame),
                // The ring answers each ACK with 0x87 and sends the next page after a fetch. With
                // nothing left, every fetch draws another 0x87; re-fetching unconditionally looped
                // at ~22 writes/s until the ring rejected writes with ATT error 0x0e.
                op::DESCRIPTOR_FETCH => {
                    if std::mem::take(&mut self.page_since_fetch) {
                        return vec![send(&cmd::FETCH)];
                    }
                    self.idle_fetches += 1;
                    if self.idle_fetches > MAX_IDLE_FETCHES {
                        return self.finish(Outcome::NoMoreData);
                    }
                    vec![Action::Send { bytes: cmd::FETCH.to_vec(), after: Duration::from_secs(1) }]
                }
                op::END_OF_HISTORY => {
                    let mut actions = vec![Action::Store(Batch::EventLog(frame.to_vec()))];
                    actions.extend(self.finish(Outcome::Complete));
                    actions
                }
                _ => Vec::new(),
            },
        }
    }

    fn on_page(&mut self, frame: &[u8]) -> Vec<Action> {
        // A page failing its checksum is never acknowledged.
        if checksum_ok(frame) != Some(true) {
            return self.finish(Outcome::Failed(format!("bad checksum on page {:02x}", frame[0])));
        }
        let Some(page) = Page::parse(frame) else {
            return self.finish(Outcome::Failed("unparseable page".into()));
        };
        let (batch, ack) = match page.op {
            op::EPOCH_PAGE => (Batch::Epochs(page.records.iter().filter_map(|r| Epoch::parse(r)).collect()), cmd::ACK_EPOCH),
            op::SPORT_PAGE => (Batch::Sport(page.records.iter().filter_map(|r| SportRecord::parse(r)).collect()), cmd::ACK_SPORT),
            _ => (Batch::Optical(page.records.iter().filter_map(|r| OpticalRecord::parse(r)).collect()), cmd::ACK_OPTICAL),
        };
        self.pages += 1;
        self.records += page.records.len();
        self.page_since_fetch = true;
        self.idle_fetches = 0;
        vec![Action::Store(batch), Action::Progress { remaining: page.remaining }, send(&ack)]
    }

    /// Called when no frame has arrived for a few seconds.
    pub fn on_quiet(&mut self) -> Vec<Action> {
        match self.phase {
            Phase::Done => Vec::new(),
            Phase::Opening => self.finish(Outcome::Failed("no reply to sync open".into())),
            Phase::Streaming => {
                self.quiet += 1;
                if self.quiet > MAX_QUIET {
                    return self.finish(Outcome::Quiet);
                }
                // Some firmware goes quiet mid-backlog without 0x50 (PROTOCOL.md section 7.4).
                vec![send(&cmd::FETCH)]
            }
        }
    }
}
