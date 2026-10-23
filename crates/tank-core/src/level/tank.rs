//! Distance → depth / percent / litres for a square (prismatic) tank.
//!
//! The sensor looks down from the lid, so distance *shrinks* as the tank
//! fills. Two calibration points define the scale:
//!
//! * `empty_mm`: distance reading when the tank is empty (sensor to floor)
//! * `full_mm`:  distance reading when the tank is full (must be ≥ the sensor
//!   blind zone, so mount the probe ≥ 50 mm above the max water line)
//!
//! Volume is linear in depth for a square tank: `litres = depth_mm * litres_per_mm`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tank {
    /// Distance reading with an empty tank, mm.
    pub empty_mm: u16,
    /// Distance reading with a full tank, mm.
    pub full_mm: u16,
    /// Litres per millimetre of depth (footprint area in m² × 1000 / 1000).
    /// For a square tank of side `s` metres: `s * s`.
    pub litres_per_mm: f32,
}

impl Default for Tank {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Tank {
    /// Placeholder until calibrated: 60 cm tank, sensor 5 cm above full,
    /// 0.5 m × 0.5 m footprint.
    pub const DEFAULT: Self = Self {
        empty_mm: 650,
        full_mm: 50,
        litres_per_mm: 0.25,
    };
}

/// A computed level reading.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub distance_mm: u16,
    pub depth_mm: u16,
    pub percent: f32,
    pub litres: f32,
}

impl Tank {
    /// Full depth of the usable water column, mm.
    pub fn full_depth_mm(&self) -> u16 {
        self.empty_mm.saturating_sub(self.full_mm)
    }

    pub fn is_calibrated(&self) -> bool {
        self.empty_mm > self.full_mm && self.litres_per_mm > 0.0
    }

    /// Convert a distance reading into a level. Values are clamped to
    /// 0..=100 % so slight over/under-shoot of the calibration points does not
    /// produce negative depth.
    pub fn level(&self, distance_mm: u16) -> Level {
        let depth_mm = self
            .empty_mm
            .saturating_sub(distance_mm)
            .min(self.full_depth_mm());
        let full = self.full_depth_mm().max(1) as f32;
        let percent = (depth_mm as f32 / full * 100.0).clamp(0.0, 100.0);
        Level {
            distance_mm,
            depth_mm,
            percent,
            litres: depth_mm as f32 * self.litres_per_mm,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_scale() {
        let t = Tank {
            empty_mm: 650,
            full_mm: 50,
            litres_per_mm: 0.25,
        };
        let l = t.level(350);
        assert_eq!(l.depth_mm, 300);
        assert!((l.percent - 50.0).abs() < 0.01);
        assert!((l.litres - 75.0).abs() < 0.01);
    }

    #[test]
    fn clamps() {
        let t = Tank::default();
        assert_eq!(t.level(900).depth_mm, 0);
        assert_eq!(t.level(10).percent, 100.0);
    }
}
