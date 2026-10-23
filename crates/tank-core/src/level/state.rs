//! Level state machine: turns a stream of levels into states and events.
//!
//! States are what the server stores; events are the edges worth alerting on.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LevelState {
    /// No valid reading yet.
    #[default]
    Unknown,
    Ok,
    Low,
    Critical,
    /// No valid frame for longer than the grace period.
    SensorFault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Low,
    Critical,
    Recovered,
    Refill,
    RapidDrop,
    SensorFault,
    SensorRecovered,
}

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub low_pct: f32,
    pub critical_pct: f32,
    /// Hysteresis band to avoid chattering around a threshold.
    pub hysteresis_pct: f32,
    /// A rise of at least this much between two samples is a refill.
    pub refill_pct: f32,
    /// A drop of at least this much between two samples is a leak / rapid drop.
    pub rapid_drop_pct: f32,
    /// Consecutive invalid samples before declaring a sensor fault.
    pub fault_after: u16,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            low_pct: 25.0,
            critical_pct: 10.0,
            hysteresis_pct: 3.0,
            refill_pct: 10.0,
            rapid_drop_pct: 10.0,
            fault_after: 6, // 60 s at a 10 s sample interval
        }
    }
}

#[derive(Debug, Default)]
pub struct StateMachine {
    state: LevelState,
    last_percent: Option<f32>,
    invalid_run: u16,
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> LevelState {
        self.state
    }

    /// Feed a valid percent reading. Returns an event if one occurred.
    pub fn update(&mut self, percent: f32, t: &Thresholds) -> Option<Event> {
        self.invalid_run = 0;
        let prev_state = self.state;
        let prev_pct = self.last_percent;
        self.last_percent = Some(percent);

        // Sensor came back.
        if prev_state == LevelState::SensorFault {
            self.state = classify(percent, t);
            return Some(Event::SensorRecovered);
        }

        // Sudden moves take priority over threshold edges.
        if let Some(p) = prev_pct {
            let delta = percent - p;
            if delta >= t.refill_pct {
                self.state = classify(percent, t);
                return Some(Event::Refill);
            }
            if -delta >= t.rapid_drop_pct {
                self.state = classify(percent, t);
                return Some(Event::RapidDrop);
            }
        }

        // Threshold edges with hysteresis.
        let next = match prev_state {
            LevelState::Unknown => classify(percent, t),
            LevelState::Ok => {
                if percent <= t.critical_pct {
                    LevelState::Critical
                } else if percent <= t.low_pct {
                    LevelState::Low
                } else {
                    LevelState::Ok
                }
            }
            LevelState::Low => {
                if percent <= t.critical_pct {
                    LevelState::Critical
                } else if percent > t.low_pct + t.hysteresis_pct {
                    LevelState::Ok
                } else {
                    LevelState::Low
                }
            }
            LevelState::Critical => {
                if percent > t.low_pct + t.hysteresis_pct {
                    LevelState::Ok
                } else if percent > t.critical_pct + t.hysteresis_pct {
                    LevelState::Low
                } else {
                    LevelState::Critical
                }
            }
            LevelState::SensorFault => unreachable!(),
        };
        self.state = next;
        match (prev_state, next) {
            (a, b) if a == b => None,
            (LevelState::Unknown, _) => None,
            (_, LevelState::Critical) => Some(Event::Critical),
            (_, LevelState::Low) if prev_state == LevelState::Ok => Some(Event::Low),
            (_, LevelState::Ok) => Some(Event::Recovered),
            _ => None,
        }
    }

    /// Feed an invalid sample (checksum error, blind zone, timeout).
    pub fn invalid(&mut self, t: &Thresholds) -> Option<Event> {
        self.invalid_run = self.invalid_run.saturating_add(1);
        if self.state != LevelState::SensorFault && self.invalid_run >= t.fault_after {
            self.state = LevelState::SensorFault;
            return Some(Event::SensorFault);
        }
        None
    }
}

fn classify(percent: f32, t: &Thresholds) -> LevelState {
    if percent <= t.critical_pct {
        LevelState::Critical
    } else if percent <= t.low_pct {
        LevelState::Low
    } else {
        LevelState::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_then_refills() {
        let t = Thresholds::default();
        let mut sm = StateMachine::new();
        assert_eq!(sm.update(80.0, &t), None);
        assert_eq!(sm.state(), LevelState::Ok);
        // Gradual drain: no events until the low threshold is crossed.
        for p in [72.0, 64.0, 56.0, 48.0, 40.0, 32.0] {
            assert_eq!(sm.update(p, &t), None);
        }
        assert_eq!(sm.update(24.0, &t), Some(Event::Low));
        assert_eq!(sm.update(27.0, &t), None); // inside hysteresis, still Low
        assert_eq!(sm.state(), LevelState::Low);
        assert_eq!(sm.update(20.0, &t), None);
        assert_eq!(sm.update(12.0, &t), None);
        assert_eq!(sm.update(9.0, &t), Some(Event::Critical));
        // Refill is a big upward jump.
        assert_eq!(sm.update(95.0, &t), Some(Event::Refill));
        assert_eq!(sm.state(), LevelState::Ok);
    }

    #[test]
    fn sudden_drop_is_a_leak() {
        let t = Thresholds::default();
        let mut sm = StateMachine::new();
        assert_eq!(sm.update(80.0, &t), None);
        assert_eq!(sm.update(60.0, &t), Some(Event::RapidDrop));
        assert_eq!(sm.state(), LevelState::Ok);
    }

    #[test]
    fn sensor_fault_and_recovery() {
        let t = Thresholds {
            fault_after: 2,
            ..Default::default()
        };
        let mut sm = StateMachine::new();
        sm.update(50.0, &t);
        assert_eq!(sm.invalid(&t), None);
        assert_eq!(sm.invalid(&t), Some(Event::SensorFault));
        assert_eq!(sm.invalid(&t), None);
        assert_eq!(sm.update(50.0, &t), Some(Event::SensorRecovered));
        assert_eq!(sm.state(), LevelState::Ok);
    }
}
