//! Colmi smart ring protocol without I/O (COLMI.md in the Ringmaster docs). Commands are 16-byte
//! packets on a UART-style service; larger history comes as variable-length "big data" packets on
//! a second service that older firmware lacks.

pub mod bigdata;
pub mod history;
pub mod live;
pub mod packet;
pub mod request;

use uuid::{uuid, Uuid};

pub const UART_SERVICE: Uuid = uuid!("6e40fff0-b5a3-f393-e0a9-e50e24dcca9e");
pub const UART_WRITE: Uuid = uuid!("6e400002-b5a3-f393-e0a9-e50e24dcca9e");
pub const UART_NOTIFY: Uuid = uuid!("6e400003-b5a3-f393-e0a9-e50e24dcca9e");
pub const DATA_SERVICE: Uuid = uuid!("de5bf728-d711-4e47-af26-65e3012a5dc7");
pub const DATA_WRITE: Uuid = uuid!("de5bf72a-d711-4e47-af26-65e3012a5dc7");
pub const DATA_NOTIFY: Uuid = uuid!("de5bf729-d711-4e47-af26-65e3012a5dc7");

pub mod cmd {
    pub const SET_TIME: u8 = 0x01;
    pub const BATTERY: u8 = 0x03;
    pub const PHONE_NAME: u8 = 0x04;
    pub const HEART_RATE_HISTORY: u8 = 0x15;
    pub const HEART_RATE_INTERVAL: u8 = 0x16;
    pub const REALTIME_HEART_RATE: u8 = 0x1e;
    pub const SPO2_AUTO: u8 = 0x2c;
    pub const STRESS_AUTO: u8 = 0x36;
    pub const STRESS_HISTORY: u8 = 0x37;
    pub const HRV_AUTO: u8 = 0x38;
    pub const HRV_HISTORY: u8 = 0x39;
    pub const TEMPERATURE_AUTO: u8 = 0x3a;
    pub const ACTIVITY_HISTORY: u8 = 0x43;
    pub const LIVE_START: u8 = 0x69;
    pub const LIVE_STOP: u8 = 0x6a;
    pub const NOTIFICATION: u8 = 0x73;
    pub const BIG_DATA: u8 = 0xbc;
}

/// Device names Gadgetbridge recognises as this protocol, by prefix.
const NAME_PREFIXES: &[&str] =
    &["R02_", "R03_", "R05_", "R06_", "R07_", "R09_", "R10_", "R11_", "R11C_", "R12_", "Y27_", "H59_", "H59MAX_", "COLMI"];

/// Whether an advertised name or service list belongs to a ring of this protocol.
pub fn is_colmi(name: &str, services: &[Uuid]) -> bool {
    NAME_PREFIXES.iter().any(|p| name.starts_with(p)) || services.contains(&UART_SERVICE) || services.contains(&DATA_SERVICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_rings() {
        assert!(is_colmi("R02_A1B2", &[]));
        assert!(is_colmi("COLMI R10_0A1B", &[]) && is_colmi("H59MAX_0A1B", &[]));
        assert!(is_colmi("", &[UART_SERVICE]));
        assert!(!is_colmi("RingConn Gen2-55BB", &[]));
    }
}
