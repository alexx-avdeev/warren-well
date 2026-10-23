//! Persistent device configuration.
//!
//! One struct, serialised with `postcard` into the `nvs` flash partition and
//! editable from BLE, the local web UI and (later) the Rails reply.

use heapless::String;
use serde::{Deserialize, Serialize};

use crate::level::tank::Tank;

/// Bump when the layout changes incompatibly; old blobs are then discarded.
pub const CONFIG_VERSION: u8 = 1;

pub type Ssid = String<32>;
pub type Password = String<64>;
pub type Url = String<128>;
pub type Token = String<64>;
pub type Name = String<32>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub version: u8,
    /// Human-readable device name (also the BLE advertised name suffix).
    pub name: Name,
    pub wifi_ssid: Ssid,
    pub wifi_password: Password,
    /// Base URL of the Rails ingest endpoint, e.g. `http://192.168.1.10:3000/api/v1/tank_readings`.
    pub api_url: Url,
    pub api_token: Token,
    pub tank: Tank,
    /// Seconds between sensor samples.
    pub sample_interval_s: u16,
    /// Seconds between periodic reports (events report immediately).
    pub report_interval_s: u16,
    pub low_pct: u8,
    pub critical_pct: u8,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl DeviceConfig {
    /// Factory defaults. `name` is empty; UIs fall back to `tank-<mac>`.
    pub const DEFAULT: Self = Self {
        version: CONFIG_VERSION,
        name: String::new(),
        wifi_ssid: String::new(),
        wifi_password: String::new(),
        api_url: String::new(),
        api_token: String::new(),
        tank: Tank::DEFAULT,
        sample_interval_s: 10,
        report_interval_s: 300,
        low_pct: 25,
        critical_pct: 10,
    };

    pub fn has_wifi(&self) -> bool {
        !self.wifi_ssid.is_empty()
    }

    pub fn has_server(&self) -> bool {
        !self.api_url.is_empty()
    }

    /// Clamp user-supplied values into sane ranges.
    pub fn sanitize(&mut self) {
        self.sample_interval_s = self.sample_interval_s.clamp(1, 3600);
        self.report_interval_s = self.report_interval_s.clamp(10, 12 * 3600);
        self.critical_pct = self.critical_pct.min(99);
        let floor = self.critical_pct.saturating_add(1).min(100);
        self.low_pct = self.low_pct.clamp(floor, 100);
    }

    pub fn thresholds(&self) -> crate::level::state::Thresholds {
        crate::level::state::Thresholds {
            low_pct: self.low_pct as f32,
            critical_pct: self.critical_pct as f32,
            ..Default::default()
        }
    }
}

/// Partial update as accepted from the web UI / BLE / server. Every field is
/// optional so a client can change one thing without resending the rest.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigPatch {
    pub name: Option<Name>,
    pub wifi_ssid: Option<Ssid>,
    pub wifi_password: Option<Password>,
    pub api_url: Option<Url>,
    pub api_token: Option<Token>,
    pub empty_mm: Option<u16>,
    pub full_mm: Option<u16>,
    pub litres_per_mm: Option<f32>,
    pub sample_interval_s: Option<u16>,
    pub report_interval_s: Option<u16>,
    pub low_pct: Option<u8>,
    pub critical_pct: Option<u8>,
}

impl ConfigPatch {
    /// Apply onto `cfg`. Returns true if WiFi credentials changed (caller
    /// should reconnect).
    pub fn apply(&self, cfg: &mut DeviceConfig) -> bool {
        let mut wifi_changed = false;
        if let Some(v) = &self.name {
            cfg.name = v.clone();
        }
        if let Some(v) = &self.wifi_ssid {
            wifi_changed |= cfg.wifi_ssid != *v;
            cfg.wifi_ssid = v.clone();
        }
        if let Some(v) = &self.wifi_password {
            wifi_changed |= cfg.wifi_password != *v;
            cfg.wifi_password = v.clone();
        }
        if let Some(v) = &self.api_url {
            cfg.api_url = v.clone();
        }
        if let Some(v) = &self.api_token {
            cfg.api_token = v.clone();
        }
        if let Some(v) = self.empty_mm {
            cfg.tank.empty_mm = v;
        }
        if let Some(v) = self.full_mm {
            cfg.tank.full_mm = v;
        }
        if let Some(v) = self.litres_per_mm {
            cfg.tank.litres_per_mm = v;
        }
        if let Some(v) = self.sample_interval_s {
            cfg.sample_interval_s = v;
        }
        if let Some(v) = self.report_interval_s {
            cfg.report_interval_s = v;
        }
        if let Some(v) = self.low_pct {
            cfg.low_pct = v;
        }
        if let Some(v) = self.critical_pct {
            cfg.critical_pct = v;
        }
        cfg.sanitize();
        wifi_changed
    }
}

/// Config as exposed to clients: same as [`DeviceConfig`] but with secrets
/// replaced by a "set / not set" flag.
#[derive(Debug, Clone, Serialize)]
pub struct PublicConfig {
    pub name: Name,
    pub wifi_ssid: Ssid,
    pub wifi_password_set: bool,
    pub api_url: Url,
    pub api_token_set: bool,
    pub empty_mm: u16,
    pub full_mm: u16,
    pub litres_per_mm: f32,
    pub sample_interval_s: u16,
    pub report_interval_s: u16,
    pub low_pct: u8,
    pub critical_pct: u8,
}

impl From<&DeviceConfig> for PublicConfig {
    fn from(c: &DeviceConfig) -> Self {
        Self {
            name: c.name.clone(),
            wifi_ssid: c.wifi_ssid.clone(),
            wifi_password_set: !c.wifi_password.is_empty(),
            api_url: c.api_url.clone(),
            api_token_set: !c.api_token.is_empty(),
            empty_mm: c.tank.empty_mm,
            full_mm: c.tank.full_mm,
            litres_per_mm: c.tank.litres_per_mm,
            sample_interval_s: c.sample_interval_s,
            report_interval_s: c.report_interval_s,
            low_pct: c.low_pct,
            critical_pct: c.critical_pct,
        }
    }
}
