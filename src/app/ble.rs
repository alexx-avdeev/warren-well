//! BLE provisioning / management GATT service (trouble-host).
//!
//! Advertises as `<name>` for five minutes after boot, whenever the BOOT
//! button is pressed, and indefinitely while no WiFi SSID is configured.
//!
//! Service `7a1e0001-5a7e-4a6b-9b0e-2b4c6d8e0f10`, characteristics:
//!
//! | # | UUID …0002.. | type           | access      | content |
//! |---|--------------|----------------|-------------|---------|
//! | 1 | …0002        | utf8 ≤32       | read/write  | WiFi SSID |
//! | 2 | …0003        | utf8 ≤64       | write       | WiFi password |
//! | 3 | …0004        | utf8 ≤128      | read/write  | API URL |
//! | 4 | …0005        | utf8 ≤64       | write       | API token |
//! | 5 | …0006        | 8 bytes LE     | read/write  | empty_mm u16, full_mm u16, litres_per_mm f32 |
//! | 6 | …0007        | 6 bytes LE     | read/write  | sample_s u16, report_s u16, low_pct u8, critical_pct u8 |
//! | 7 | …0008        | utf8 ≤160 JSON | read/notify | live status |
//! | 8 | …0009        | u8             | write       | command: 1 save+apply, 2 reboot, 3 cal full, 4 cal empty, 5 factory reset, 6 test report, 7 re-enrol |
//!
//! Writes to 1-6 change the in-memory config immediately; command 1 persists
//! them and reconnects WiFi.

use bt_hci::controller::ExternalController;
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use esp_radio::ble::controller::BleConnector;
use heapless::String;
use log::{info, warn};
use trouble_host::prelude::*;

use crate::app::state::{BLE_WINDOW, COMMANDS, CONFIG, Command, LEVEL, NET, display_name};

pub type Controller = ExternalController<BleConnector<'static>, 1>;

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2; // signal + att
const WINDOW: Duration = Duration::from_secs(5 * 60);
/// 7a1e0001-5a7e-4a6b-9b0e-2b4c6d8e0f10 as little-endian bytes for advertising.
const SERVICE_UUID_LE: [u8; 16] = [
    0x10, 0x0f, 0x8e, 0x6d, 0x4c, 0x2b, 0x0e, 0x9b, 0x6b, 0x4a, 0x7e, 0x5a, 0x01, 0x00, 0x1e, 0x7a,
];

#[gatt_server]
struct Server {
    tank: TankService,
}

#[gatt_service(uuid = "7a1e0001-5a7e-4a6b-9b0e-2b4c6d8e0f10")]
struct TankService {
    #[characteristic(uuid = "7a1e0002-5a7e-4a6b-9b0e-2b4c6d8e0f10", read, write)]
    wifi_ssid: String<32>,
    #[characteristic(uuid = "7a1e0003-5a7e-4a6b-9b0e-2b4c6d8e0f10", write)]
    wifi_password: String<64>,
    #[characteristic(uuid = "7a1e0004-5a7e-4a6b-9b0e-2b4c6d8e0f10", read, write)]
    api_url: String<128>,
    #[characteristic(uuid = "7a1e0005-5a7e-4a6b-9b0e-2b4c6d8e0f10", write)]
    api_token: String<64>,
    #[characteristic(uuid = "7a1e0006-5a7e-4a6b-9b0e-2b4c6d8e0f10", read, write)]
    tank_cal: [u8; 8],
    #[characteristic(uuid = "7a1e0007-5a7e-4a6b-9b0e-2b4c6d8e0f10", read, write)]
    intervals: [u8; 6],
    #[characteristic(uuid = "7a1e0008-5a7e-4a6b-9b0e-2b4c6d8e0f10", read, notify)]
    status: String<160>,
    #[characteristic(uuid = "7a1e0009-5a7e-4a6b-9b0e-2b4c6d8e0f10", write)]
    command: u8,
}

#[embassy_executor::task]
pub async fn ble_task(controller: Controller, mac: [u8; 6]) -> ! {
    // Static random address derived from the MAC (top two bits must be 1).
    let mut addr = mac;
    addr.reverse();
    addr[5] |= 0xC0;
    let address = Address::random(addr);

    let mut resources: HostResources<_, DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .build();
    let mut peripheral = stack.peripheral();
    let runner = stack.runner();

    let name = display_name().await;
    let server = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: &name,
        appearance: &appearance::sensor::GENERIC_SENSOR,
    }))
    .expect("gatt server");

    let _ = join(ble_runner(runner), advertise_loop(&mut peripheral, &server, &name)).await;
    unreachable!()
}

async fn ble_runner<C: trouble_host::Controller>(mut runner: Runner<'_, C, DefaultPacketPool>) {
    info!("ble: host runner started");
    loop {
        if let Err(e) = runner.run().await {
            warn!("ble: runner error: {e:?}");
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}

/// True while BLE should be discoverable.
async fn window_open(deadline: &mut Instant) -> bool {
    if Instant::now() < *deadline {
        return true;
    }
    // Stay discoverable while unprovisioned.
    if CONFIG.lock().await.wifi_ssid.is_empty() {
        return true;
    }
    false
}

async fn advertise_loop<C: trouble_host::Controller>(
    peripheral: &mut Peripheral<'_, C, DefaultPacketPool>,
    server: &Server<'_>,
    name: &str,
) {
    let mut deadline = Instant::now() + WINDOW;

    let mut adv_data = [0u8; 31];
    let adv_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut adv_data,
    )
    .expect("adv data");
    let mut scan_data = [0u8; 31];
    let scan_len = AdStructure::encode_slice(
        &[AdStructure::CompleteServiceUuids128(&[SERVICE_UUID_LE])],
        &mut scan_data,
    )
    .expect("scan data");

    loop {
        if !window_open(&mut deadline).await {
            info!("ble: window closed, waiting for button / request");
            BLE_WINDOW.wait().await;
            deadline = Instant::now() + WINDOW;
        }
        info!("ble: advertising as '{name}'");

        let params = AdvertisementParameters {
            interval_min: Duration::from_millis(200),
            interval_max: Duration::from_millis(400),
            ..Default::default()
        };
        let adv = peripheral.advertise(
            &params,
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..adv_len],
                scan_data: &scan_data[..scan_len],
            },
        );
        let until = Timer::at(deadline);
        let acceptor = match select(adv, async {
            // While unprovisioned the deadline is irrelevant; park this branch.
            if CONFIG.lock().await.wifi_ssid.is_empty() {
                core::future::pending::<()>().await;
            }
            until.await;
        })
        .await
        {
            Either::First(Ok(acceptor)) => acceptor,
            Either::First(Err(e)) => {
                warn!("ble: advertise error: {e:?}");
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
            Either::Second(()) => {
                info!("ble: advertising window expired");
                continue; // loop re-checks the window
            }
        };
        info!("ble: advertising started, waiting for a central");

        match acceptor.accept().await {
            Ok(conn) => match conn.with_attribute_server(server) {
                Ok(conn) => {
                    info!("ble: central connected");
                    gatt_session(server, &conn).await;
                    // A connection extends the window so the user can finish up.
                    deadline = Instant::now() + WINDOW;
                }
                Err(e) => warn!("ble: attribute server error: {e:?}"),
            },
            Err(e) => warn!("ble: accept error: {e:?}"),
        }
    }
}

/// Serve one connection until it drops. Pushes config writes into `CONFIG`
/// and streams status notifications when the level changes.
async fn gatt_session(server: &Server<'_>, conn: &GattConnection<'_, '_, DefaultPacketPool>) {
    let svc = &server.tank;
    // Preload readable characteristics from the live config.
    {
        let cfg = CONFIG.lock().await;
        let _ = server.set(&svc.wifi_ssid, &cfg.wifi_ssid);
        let _ = server.set(&svc.api_url, &cfg.api_url);
        let _ = server.set(&svc.tank_cal, &encode_tank_cal(&cfg));
        let _ = server.set(&svc.intervals, &encode_intervals(&cfg));
    }
    let _ = server.set(&svc.status, &status_json().await);
    let mut level_rx = LEVEL.receiver().unwrap();

    loop {
        match select(conn.next(), level_rx.changed()).await {
            Either::Second(_) => {
                let s = status_json().await;
                let _ = server.set(&svc.status, &s);
                let _ = svc.status.notify(conn, &s, true).await;
            }
            Either::First(GattConnectionEvent::Disconnected { reason }) => {
                info!("ble: disconnected: {reason:?}");
                return;
            }
            Either::First(GattConnectionEvent::Gatt { event }) => {
                if let GattEvent::Write(w) = &event {
                    handle_write(svc, w).await;
                }
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(e) => warn!("ble: reply error: {e:?}"),
                }
            }
            Either::First(_) => {}
        }
    }
}

async fn handle_write(svc: &TankService, w: &WriteEvent<'_, '_, DefaultPacketPool>) {
    let h = w.handle();
    if h == svc.wifi_ssid.handle {
        if let Ok(v) = w.value(&svc.wifi_ssid) {
            info!("ble: wifi ssid set to '{v}'");
            CONFIG.lock().await.wifi_ssid = v;
        }
    } else if h == svc.wifi_password.handle {
        if let Ok(v) = w.value(&svc.wifi_password) {
            info!("ble: wifi password set ({} chars)", v.len());
            CONFIG.lock().await.wifi_password = v;
        }
    } else if h == svc.api_url.handle {
        if let Ok(v) = w.value(&svc.api_url) {
            info!("ble: api url set to '{v}'");
            CONFIG.lock().await.api_url = v;
        }
    } else if h == svc.api_token.handle {
        if let Ok(v) = w.value(&svc.api_token) {
            info!("ble: api token set ({} chars)", v.len());
            CONFIG.lock().await.api_token = v;
        }
    } else if h == svc.tank_cal.handle {
        if let Ok(b) = w.value(&svc.tank_cal) {
            let mut cfg = CONFIG.lock().await;
            cfg.tank.empty_mm = u16::from_le_bytes([b[0], b[1]]);
            cfg.tank.full_mm = u16::from_le_bytes([b[2], b[3]]);
            cfg.tank.litres_per_mm = f32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            info!("ble: tank cal {:?}", cfg.tank);
        }
    } else if h == svc.intervals.handle {
        if let Ok(b) = w.value(&svc.intervals) {
            let mut cfg = CONFIG.lock().await;
            cfg.sample_interval_s = u16::from_le_bytes([b[0], b[1]]);
            cfg.report_interval_s = u16::from_le_bytes([b[2], b[3]]);
            cfg.low_pct = b[4];
            cfg.critical_pct = b[5];
            cfg.sanitize();
            info!(
                "ble: intervals sample={} report={} low={} crit={}",
                cfg.sample_interval_s, cfg.report_interval_s, cfg.low_pct, cfg.critical_pct
            );
        }
    } else if h == svc.command.handle
        && let Ok(code) = w.value(&svc.command)
    {
        match Command::from_code(code) {
            Some(cmd) => {
                let _ = COMMANDS.try_send(cmd);
            }
            None => warn!("ble: unknown command {code}"),
        }
    }
}

fn encode_tank_cal(cfg: &tank_core::config::DeviceConfig) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0..2].copy_from_slice(&cfg.tank.empty_mm.to_le_bytes());
    b[2..4].copy_from_slice(&cfg.tank.full_mm.to_le_bytes());
    b[4..8].copy_from_slice(&cfg.tank.litres_per_mm.to_le_bytes());
    b
}

fn encode_intervals(cfg: &tank_core::config::DeviceConfig) -> [u8; 6] {
    let mut b = [0u8; 6];
    b[0..2].copy_from_slice(&cfg.sample_interval_s.to_le_bytes());
    b[2..4].copy_from_slice(&cfg.report_interval_s.to_le_bytes());
    b[4] = cfg.low_pct;
    b[5] = cfg.critical_pct;
    b
}

/// Compact status JSON for the BLE characteristic (kept ≤160 bytes).
async fn status_json() -> String<160> {
    use core::fmt::Write;
    let snap = LEVEL.try_get().unwrap_or_default();
    let net = NET.try_get().unwrap_or_default();
    let mut s: String<160> = String::new();
    let _ = write!(s, "{{\"state\":\"{:?}\"", snap.state);
    if let Some(l) = snap.level {
        let _ = write!(
            s,
            ",\"mm\":{},\"pct\":{:.1},\"l\":{:.1}",
            l.distance_mm, l.percent, l.litres
        );
    }
    let _ = write!(s, ",\"wifi\":{}", net.connected);
    let rep = crate::app::state::REPORT.try_get().unwrap_or_default();
    let srv = match rep.last_error {
        None if rep.sent > 0 => "ok",
        None => "idle",
        Some(crate::app::state::ReportError::PendingApproval) => "pending",
        Some(crate::app::state::ReportError::Unauthorized) => "unauth",
        Some(crate::app::state::ReportError::NoServer) => "none",
        Some(_) => "err",
    };
    let _ = write!(s, ",\"srv\":\"{srv}\"");
    if let Some(ip) = net.ip {
        let _ = write!(s, ",\"ip\":\"{ip}\"");
    }
    if let Some(r) = net.rssi {
        let _ = write!(s, ",\"rssi\":{r}");
    }
    let _ = s.push('}');
    s
}
