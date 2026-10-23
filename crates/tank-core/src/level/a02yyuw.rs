//! Frame parser for the DFRobot A02YYUW ultrasonic sensor.
//!
//! The sensor streams 4-byte frames over UART (9600 8N1):
//!
//! | byte | meaning                         |
//! |------|---------------------------------|
//! | 0    | header, always `0xFF`           |
//! | 1    | distance high byte              |
//! | 2    | distance low byte               |
//! | 3    | checksum = (0xFF + hi + lo) & 0xFF |
//!
//! Distance is in millimetres. Readings inside the 30 mm blind zone are
//! reported by the sensor but are not trustworthy, so they are flagged.

pub const HEADER: u8 = 0xFF;
pub const FRAME_LEN: usize = 4;
/// Sensor cannot measure closer than this.
pub const BLIND_ZONE_MM: u16 = 30;
/// Datasheet maximum range.
pub const MAX_RANGE_MM: u16 = 4500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Checksum did not match; the frame is dropped and the parser resyncs.
    Checksum,
    /// Distance below the blind zone; the value is unreliable.
    BlindZone(u16),
    /// Distance above the sensor's rated range.
    OutOfRange(u16),
}

/// Byte-at-a-time parser that resynchronises on the header byte.
#[derive(Debug, Default, Clone)]
pub struct Parser {
    buf: [u8; FRAME_LEN],
    len: usize,
}

impl Parser {
    pub const fn new() -> Self {
        Self {
            buf: [0; FRAME_LEN],
            len: 0,
        }
    }

    /// Feed one byte. Returns `Some` when a complete frame has been consumed.
    pub fn push(&mut self, byte: u8) -> Option<Result<u16, FrameError>> {
        if self.len == 0 {
            if byte != HEADER {
                return None;
            }
            self.buf[0] = byte;
            self.len = 1;
            return None;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len < FRAME_LEN {
            return None;
        }
        self.len = 0;
        Some(decode(&self.buf))
    }

    /// Feed a slice, invoking `f` for every completed frame.
    pub fn push_slice(&mut self, bytes: &[u8], mut f: impl FnMut(Result<u16, FrameError>)) {
        for &b in bytes {
            if let Some(r) = self.push(b) {
                f(r);
            }
        }
    }
}

/// Decode one aligned 4-byte frame.
pub fn decode(frame: &[u8; FRAME_LEN]) -> Result<u16, FrameError> {
    let sum = frame[0].wrapping_add(frame[1]).wrapping_add(frame[2]);
    if frame[0] != HEADER || sum != frame[3] {
        return Err(FrameError::Checksum);
    }
    let mm = u16::from_be_bytes([frame[1], frame[2]]);
    if mm < BLIND_ZONE_MM {
        Err(FrameError::BlindZone(mm))
    } else if mm > MAX_RANGE_MM {
        Err(FrameError::OutOfRange(mm))
    } else {
        Ok(mm)
    }
}

/// Build a valid frame for a distance (used by the simulator and tests).
pub fn encode(mm: u16) -> [u8; FRAME_LEN] {
    let [hi, lo] = mm.to_be_bytes();
    [HEADER, hi, lo, HEADER.wrapping_add(hi).wrapping_add(lo)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datasheet_example() {
        // From the DFRobot wiki: FF 07 A1 A7 → 1953 mm
        assert_eq!(decode(&[0xFF, 0x07, 0xA1, 0xA7]), Ok(1953));
    }

    #[test]
    fn bad_checksum() {
        assert_eq!(decode(&[0xFF, 0x07, 0xA1, 0xA8]), Err(FrameError::Checksum));
    }

    #[test]
    fn resync_after_garbage() {
        let mut p = Parser::new();
        let mut out = heapless::Vec::<_, 4>::new();
        let mut bytes = heapless::Vec::<u8, 16>::new();
        bytes.extend_from_slice(&[0x12, 0xA1]).unwrap();
        bytes.extend_from_slice(&encode(1953)).unwrap();
        bytes.extend_from_slice(&encode(312)).unwrap();
        p.push_slice(&bytes, |r| out.push(r).unwrap());
        assert_eq!(out.as_slice(), &[Ok(1953), Ok(312)]);
    }

    #[test]
    fn blind_zone_flagged() {
        assert_eq!(decode(&encode(20)), Err(FrameError::BlindZone(20)));
        assert_eq!(decode(&encode(30)), Ok(30));
    }
}
