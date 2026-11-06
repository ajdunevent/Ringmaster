//! Drains a simulated ring that serves real Gen 2 pages with the fetch/ACK behaviour recorded by
//! ringprobe on 2026-10-21.

use ringconn::drain::{Action, Batch, ChannelDrain, Outcome};
use ringconn::frame::{op, parse_hex};
use ringconn::Channel;
use std::collections::VecDeque;

const DESCRIPTOR: &str = "87 49 02 00 00 39 01 0a 01 0d 00 00 00 00 0f fa 5b ff a3";

fn fixture() -> (Vec<Vec<u8>>, Vec<u8>) {
    let text = include_str!("fixtures/gen2-fr02018-sleep-channel.hex");
    let mut frames: Vec<Vec<u8>> = text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| parse_hex(l).unwrap()).collect();
    let end = frames.pop().unwrap();
    assert_eq!(end[0], op::END_OF_HISTORY);
    (frames, end)
}

/// Observed behaviour: the first fetch after a sync open draws only a descriptor; each later fetch
/// draws the next page; each ACK draws a descriptor; once pages run out a fetch draws `0x50` once,
/// then descriptors forever.
struct FakeRing {
    pages: VecDeque<Vec<u8>>,
    end: Option<Vec<u8>>,
    primed: bool,
    awaiting_ack: bool,
    acked: usize,
    writes: usize,
}

impl FakeRing {
    fn new(pages: Vec<Vec<u8>>, end: Option<Vec<u8>>) -> Self {
        Self { pages: pages.into(), end, primed: false, awaiting_ack: false, acked: 0, writes: 0 }
    }

    fn write(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.writes += 1;
        let descriptor = parse_hex(DESCRIPTOR).unwrap();
        match bytes[0] {
            0x02 => {
                self.primed = false;
                vec![vec![0x82, 0x00, 0x00, 0x82]]
            }
            0x07 if !self.primed => {
                self.primed = true;
                vec![descriptor]
            }
            0x07 if self.awaiting_ack => vec![descriptor],
            0x07 => match self.pages.front() {
                Some(p) => {
                    self.awaiting_ack = true;
                    vec![p.clone()]
                }
                None => self.end.take().map(|e| vec![e]).unwrap_or_else(|| vec![descriptor]),
            },
            0xc7 | 0xcc | 0xcd => {
                assert!(self.awaiting_ack, "ACK without an outstanding page");
                self.awaiting_ack = false;
                self.pages.pop_front();
                self.acked += 1;
                vec![descriptor]
            }
            other => panic!("unexpected command {other:02x}"),
        }
    }
}

struct Run {
    stored_pages: usize,
    epochs: Vec<u32>,
    outcome: Outcome,
}

fn drain(ring: &mut FakeRing) -> Run {
    let (mut machine, actions) = ChannelDrain::start(Channel::Sleep, 1_792_000_000);
    let mut queue: VecDeque<Action> = actions.into();
    let mut inbox: VecDeque<Vec<u8>> = VecDeque::new();
    let (mut stored_pages, mut epochs, mut outcome) = (0, Vec::new(), None);
    let mut stored_since_ack = 0usize;

    for _ in 0..10_000 {
        if let Some(action) = queue.pop_front() {
            match action {
                Action::Store(batch) => {
                    if let Batch::Epochs(e) = &batch {
                        epochs.extend(e.iter().map(|e| e.counter));
                    }
                    if !matches!(batch, Batch::EventLog(_)) {
                        stored_pages += 1;
                        stored_since_ack += 1;
                    }
                }
                Action::Send { bytes, .. } => {
                    if matches!(bytes[0], 0xc7 | 0xcc | 0xcd) {
                        assert_eq!(stored_since_ack, 1, "every ACK must follow exactly one stored page");
                        stored_since_ack = 0;
                    }
                    inbox.extend(ring.write(&bytes));
                }
                Action::Progress { .. } => {}
                Action::Finished(o) => outcome = Some(o),
            }
            continue;
        }
        if machine.is_done() {
            break;
        }
        match inbox.pop_front() {
            Some(frame) => queue.extend(machine.on_frame(&frame)),
            None => queue.extend(machine.on_quiet()),
        }
    }
    Run { stored_pages, epochs, outcome: outcome.expect("drain did not finish") }
}

#[test]
fn drains_every_page_once_and_stops_on_end_of_history() {
    let (pages, end) = fixture();
    let page_count = pages.len();
    let mut ring = FakeRing::new(pages, Some(end));
    let run = drain(&mut ring);

    assert_eq!(run.outcome, Outcome::Complete);
    assert_eq!(run.stored_pages, page_count);
    assert_eq!(ring.acked, page_count);
    let mut sorted = run.epochs.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), run.epochs.len(), "an epoch was stored twice");
    assert!(run.epochs.windows(2).all(|w| w[0] < w[1]), "epochs out of order");
}

#[test]
fn empty_channel_stops_after_a_few_fetches() {
    let mut ring = FakeRing::new(Vec::new(), None);
    let run = drain(&mut ring);
    assert_eq!(run.outcome, Outcome::NoMoreData);
    assert!(ring.writes <= 6, "{} writes to an empty channel", ring.writes);
}

#[test]
fn corrupt_page_is_not_acknowledged() {
    let (mut pages, end) = fixture();
    let last = pages[0].len() - 1;
    pages[0][last] ^= 0xff;
    let mut ring = FakeRing::new(pages, Some(end));
    let run = drain(&mut ring);
    assert!(matches!(run.outcome, Outcome::Failed(_)));
    assert_eq!(ring.acked, 0);
}

#[test]
fn records_date_before_the_sync_that_fetched_them() {
    let (pages, _) = fixture();
    let newest = pages
        .iter()
        .filter(|p| p[0] == op::EPOCH_PAGE)
        .flat_map(|p| {
            ringconn::records::Page::parse(p).unwrap().records.into_iter().map(|r| u32::from_be_bytes(r[..4].try_into().unwrap()))
        })
        .max()
        .unwrap();
    // The sync was opened at 2026-10-21 08:33:41 CDT.
    let sync = 1_792_589_621;
    let age = sync - ringconn::time::to_unix(newest);
    assert!((0..300).contains(&age), "newest record is {age} s before the sync");
}
