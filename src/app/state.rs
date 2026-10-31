//! Shared state between tasks. All of it is `static` so any task can reach it
//! without threading references through constructors.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_net::Ipv4Address;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as Raw;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use heapless::String;
use tank_core::config::DeviceConfig;
use tank_core::level::{Event, Level, LevelState};

/// The live configuration. Persist with [`Command::SaveConfig`].
pub static CONFIG: Mutex<Raw, DeviceConfig> = Mutex::new(DeviceConfig::DEFAULT);

/// Device id derived from the WiFi MAC (12 hex chars). Set once at boot.
pub static DEVICE_ID: Mutex<Raw, String<12>> = Mutex::new(String::new());

/// Latest level snapshot, updated by the sampler on every sample.
pub static LEVEL: Watch<Raw, LevelSnapshot, 4> = Watch::new();

/// Network status, updated by the WiFi task.
pub static NET: Watch<Raw, NetStatus, 4> = Watch::new();

/// Level events the reporter should send immediately.
pub static EVENTS: Channel<Raw, Event, 8> = Channel::new();

/// Commands from the UIs to the control task.
pub static COMMANDS: Channel<Raw, Command, 4> = Channel::new();

/// Ask the WiFi task to drop and reconnect with the current credentials.
pub static WIFI_RECONNECT: Signal<Raw, ()> = Signal::new();

/// Open (or extend) the BLE provisioning window.
pub static BLE_WINDOW: Signal<Raw, ()> = Signal::new();

/// Ask the reporter to send a report now.
pub static REPORT_NOW: Signal<Raw, ()> = Signal::new();

/// Random id of this boot, set once in `main`.
pub static BOOT_ID: AtomicU32 = AtomicU32::new(0);

/// Unix time at boot (`server_time - uptime`) learned from the server; 0 = unknown.
/// u32 is fine until 2106 and avoids 64-bit atomics on RV32.
pub static TIME_OFFSET: AtomicU32 = AtomicU32::new(0);

/// Outcome of the last report attempt, for the UIs.
pub static REPORT: Watch<Raw, ReportStatus, 4> = Watch::new();

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportStatus {
    pub last_ok_uptime_s: Option<u64>,
    pub last_error: Option<ReportError>,
    pub queued: u8,
    pub sent: u32,
    /// Device holds a token (approved or pending).
    pub enrolled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportError {
    NoServer,
    Offline,
    Connect,
    /// Enrolled, waiting for an admin to approve the device on the server.
    PendingApproval,
    /// Enrolment itself failed (bad reply or rejected); will retry.
    EnrolFailed,
    Unauthorized,
    RateLimited,
    Rejected(u16),
    ServerError(u16),
    Timeout,
}

/// Unix seconds now, if the server has told us the time.
pub fn unix_now() -> Option<u64> {
    let off = TIME_OFFSET.load(Ordering::Relaxed);
    (off != 0).then(|| off as u64 + uptime_s())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LevelSnapshot {
    /// Last raw median distance, even if the tank maths is uncalibrated.
    pub raw_mm: Option<u16>,
    pub level: Option<Level>,
    pub state: LevelState,
    pub last_event: Option<Event>,
    /// Monotonic sample counter.
    pub sample_no: u32,
    /// Uptime (seconds) at which the sample was taken.
    pub uptime_s: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetStatus {
    pub connected: bool,
    pub ip: Option<Ipv4Address>,
    pub rssi: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Persist the in-memory config; optionally reconnect WiFi afterwards.
    SaveConfig { reconnect_wifi: bool },
    Reboot,
    /// Record the current distance as the "full" calibration point.
    CalibrateFull,
    /// Record the current distance as the "empty" calibration point.
    CalibrateEmpty,
    FactoryReset,
    TestReport,
    OpenBleWindow,
    /// Forget the server token and enrol again.
    Enroll,
}

impl Command {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "save" => Command::SaveConfig {
                reconnect_wifi: false,
            },
            "save_reconnect" => Command::SaveConfig {
                reconnect_wifi: true,
            },
            "reboot" => Command::Reboot,
            "calibrate_full" => Command::CalibrateFull,
            "calibrate_empty" => Command::CalibrateEmpty,
            "factory_reset" => Command::FactoryReset,
            "test_report" => Command::TestReport,
            "ble" => Command::OpenBleWindow,
            "enroll" => Command::Enroll,
            _ => return None,
        })
    }

    /// Numeric encoding used by the BLE command characteristic.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Command::SaveConfig {
                reconnect_wifi: true,
            },
            2 => Command::Reboot,
            3 => Command::CalibrateFull,
            4 => Command::CalibrateEmpty,
            5 => Command::FactoryReset,
            6 => Command::TestReport,
            7 => Command::Enroll,
            _ => return None,
        })
    }
}

/// Seconds since boot.
pub fn uptime_s() -> u64 {
    embassy_time::Instant::now().as_secs()
}

/// Advertised / displayed device name: configured name or `tank-<last 4 of mac>`.
pub async fn display_name() -> String<32> {
    let cfg = CONFIG.lock().await;
    if !cfg.name.is_empty() {
        return cfg.name.clone();
    }
    drop(cfg);
    let id = DEVICE_ID.lock().await;
    let mut s: String<32> = String::new();
    let _ = s.push_str("tank-");
    let _ = s.push_str(&id[id.len().saturating_sub(4)..]);
    s
}
