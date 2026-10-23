//! JSON report sent to the server and status shown in the UIs.

use heapless::String;
use serde::Serialize;

use crate::level::{Event, Level, LevelState};

/// One report. Serialised with `serde-json-core`; floats are printed with
/// full precision which is fine for a few hundred bytes every 5 minutes.
#[derive(Debug, Clone, Serialize)]
pub struct Report<'a> {
    pub device_id: &'a str,
    pub seq: u32,
    pub fw: &'a str,
    pub uptime_s: u64,
    pub rssi: Option<i32>,
    pub level: LevelReport,
    pub sampled_at_uptime_s: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LevelReport {
    pub distance_mm: Option<u16>,
    pub depth_mm: Option<u16>,
    pub percent: Option<f32>,
    pub litres: Option<f32>,
    pub state: LevelState,
    pub event: Option<Event>,
}

impl LevelReport {
    pub fn from_level(level: Option<Level>, state: LevelState, event: Option<Event>) -> Self {
        Self {
            distance_mm: level.map(|l| l.distance_mm),
            depth_mm: level.map(|l| l.depth_mm),
            percent: level.map(|l| l.percent),
            litres: level.map(|l| l.litres),
            state,
            event,
        }
    }
}

/// Format a 6-byte MAC as 12 lowercase hex chars (the device id).
pub fn device_id(mac: &[u8; 6]) -> String<12> {
    let mut s = String::new();
    for b in mac {
        let hi = b"0123456789abcdef"[(b >> 4) as usize] as char;
        let lo = b"0123456789abcdef"[(b & 0xF) as usize] as char;
        let _ = s.push(hi);
        let _ = s.push(lo);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_formats() {
        assert_eq!(
            device_id(&[0xf0, 0xf5, 0xbd, 0x0b, 0xec, 0xe4]).as_str(),
            "f0f5bd0bece4"
        );
    }
}
