//! Local HTTP UI and JSON API (picoserve), served on port 80 over the LAN.
//!
//! * `GET  /`             – single-page UI (embedded HTML)
//! * `GET  /api/status`   – live level / network status
//! * `GET  /api/config`   – config with secrets masked
//! * `POST /api/config`   – JSON `ConfigPatch`; applies, persists, reconnects WiFi if needed
//! * `POST /api/command`  – `{"command":"reboot|calibrate_full|calibrate_empty|factory_reset|test_report|ble"}`
//!
//! No authentication yet: the device is only reachable on the farm LAN.

use embassy_net::Stack;
use embassy_time::Duration;
use heapless::String;
use log::info;
use picoserve::extract::Json;
use picoserve::response::{IntoResponse, Response};
use picoserve::routing::{PathRouter, get, post};
use serde::{Deserialize, Serialize};
use tank_core::FIRMWARE_VERSION;
use tank_core::config::{ConfigPatch, PublicConfig};
use tank_core::level::{Event, Level, LevelState};

use crate::app::state::{COMMANDS, CONFIG, Command, DEVICE_ID, LEVEL, NET, REPORT, ReportError, display_name, unix_now, uptime_s};

const INDEX_HTML: &str = include_str!("web/index.html");

pub const WEB_TASK_POOL_SIZE: usize = 2;

#[derive(Serialize)]
struct Status {
    device_id: String<12>,
    name: String<32>,
    fw: &'static str,
    uptime_s: u64,
    wifi_connected: bool,
    ip: Option<String<15>>,
    rssi: Option<i32>,
    state: LevelState,
    last_event: Option<Event>,
    raw_mm: Option<u16>,
    level: Option<Level>,
    sample_no: u32,
    sampled_uptime_s: u64,
    calibrated: bool,
    unix_time: Option<u64>,
    report: ReportInfo,
}

#[derive(Serialize)]
struct ReportInfo {
    sent: u32,
    queued: u8,
    last_ok_uptime_s: Option<u64>,
    last_error: Option<String<24>>,
}

fn report_error_name(e: ReportError) -> String<24> {
    use core::fmt::Write;
    let mut s = String::new();
    let _ = match e {
        ReportError::NoServer => s.push_str("no_server").ok(),
        ReportError::Offline => s.push_str("offline").ok(),
        ReportError::Connect => s.push_str("connect_failed").ok(),
        ReportError::Unauthorized => s.push_str("unauthorized").ok(),
        ReportError::Timeout => s.push_str("timeout").ok(),
        ReportError::Rejected(c) => write!(s, "rejected_{c}").ok(),
        ReportError::ServerError(c) => write!(s, "server_error_{c}").ok(),
    };
    s
}

#[derive(Deserialize)]
struct CommandRequest {
    command: String<24>,
}

#[derive(Serialize)]
struct Ack {
    ok: bool,
    message: &'static str,
}

async fn status() -> impl IntoResponse {
    let snap = LEVEL.try_get().unwrap_or_default();
    let net = NET.try_get().unwrap_or_default();
    let ip = net.ip.map(|ip| {
        use core::fmt::Write;
        let mut s: String<15> = String::new();
        let _ = write!(s, "{ip}");
        s
    });
    let calibrated = CONFIG.lock().await.tank.is_calibrated();
    let rep = REPORT.try_get().unwrap_or_default();
    // Take each lock in its own statement: a guard created inside the struct
    // literal would live for the whole expression and deadlock `display_name`.
    let device_id = DEVICE_ID.lock().await.clone();
    let name = display_name().await;
    Json(Status {
        device_id,
        name,
        fw: FIRMWARE_VERSION,
        uptime_s: uptime_s(),
        wifi_connected: net.connected,
        ip,
        rssi: net.rssi,
        state: snap.state,
        last_event: snap.last_event,
        raw_mm: snap.raw_mm,
        level: snap.level,
        sample_no: snap.sample_no,
        sampled_uptime_s: snap.uptime_s,
        calibrated,
        unix_time: unix_now(),
        report: ReportInfo {
            sent: rep.sent,
            queued: rep.queued,
            last_ok_uptime_s: rep.last_ok_uptime_s,
            last_error: rep.last_error.map(report_error_name),
        },
    })
}

async fn get_config() -> impl IntoResponse {
    let cfg = CONFIG.lock().await;
    Json(PublicConfig::from(&*cfg))
}

async fn patch_config(Json(patch): Json<ConfigPatch>) -> impl IntoResponse {
    let (public, wifi_changed) = {
        let mut cfg = CONFIG.lock().await;
        let wifi_changed = patch.apply(&mut cfg);
        (PublicConfig::from(&*cfg), wifi_changed)
    };
    info!("web: config updated (wifi changed: {wifi_changed})");
    let _ = COMMANDS
        .send(Command::SaveConfig {
            reconnect_wifi: wifi_changed,
        })
        .await;
    Json(public)
}

async fn command(Json(req): Json<CommandRequest>) -> impl IntoResponse {
    match Command::parse(&req.command) {
        Some(cmd) => {
            COMMANDS.send(cmd).await;
            Json(Ack {
                ok: true,
                message: "accepted",
            })
        }
        None => Json(Ack {
            ok: false,
            message: "unknown command",
        }),
    }
}

async fn index() -> impl IntoResponse {
    Response::ok(INDEX_HTML).with_header("Content-Type", "text/html; charset=utf-8")
}

fn make_app() -> picoserve::Router<impl PathRouter> {
    picoserve::Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/config", get(get_config).post(patch_config))
        .route("/api/command", post(command))
}

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
pub async fn web_task(id: usize, stack: Stack<'static>) -> ! {
    let app = make_app();
    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: Duration::from_secs(5),
        persistent_start_read_request: Duration::from_secs(2),
        read_request: Duration::from_secs(2),
        write: Duration::from_secs(2),
    })
    .keep_connection_alive();

    let mut tcp_rx = [0u8; 1024];
    let mut tcp_tx = [0u8; 1536];
    let mut http_buf = [0u8; 2048];

    if id == 0 {
        info!("web: listening on port 80");
    }
    picoserve::Server::new(&app, &config, &mut http_buf)
        .listen_and_serve(id, stack, 80, &mut tcp_rx, &mut tcp_tx)
        .await
        .into_never()
}
