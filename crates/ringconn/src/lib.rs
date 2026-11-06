//! RingConn smart ring protocol without I/O. Section numbers refer to PROTOCOL.md.

pub mod auth;
pub mod drain;
pub mod frame;
pub mod records;
pub mod time;

use uuid::{uuid, Uuid};

pub const DATA_SERVICE: Uuid = uuid!("8327ad99-2d87-4a22-a8ce-6dd7971c0437");
pub const COMMAND_CHAR: Uuid = uuid!("8327ad98-2d87-4a22-a8ce-6dd7971c0437");
pub const NOTIFY_CHAR: Uuid = uuid!("8327ad97-2d87-4a22-a8ce-6dd7971c0437");

/// History channels, each with its own resume pointer on the ring (section 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Sleep = 0x00,
    Sport = 0x02,
    AllDay = 0x03,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Sleep, Channel::Sport, Channel::AllDay];

    pub fn id(self) -> u8 {
        self as u8
    }
}
