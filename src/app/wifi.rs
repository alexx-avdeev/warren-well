//! WiFi station management and the embassy-net stack runner.

use embassy_futures::select::{Either, Either3, select3};
use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, Interface, PowerSaveMode, WifiController};
use log::{info, warn};

use crate::app::state::{CONFIG, NET, NetStatus, WIFI_RECONNECT};

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Keeps the station connected to whatever credentials are in `CONFIG`.
/// Waits idle (BLE/web still work) when no SSID is configured.
#[embassy_executor::task]
pub async fn connection_task(mut controller: WifiController<'static>, stack: Stack<'static>) -> ! {
    let net = NET.sender();
    net.send(NetStatus::default());

    loop {
        let (ssid, password) = {
            let cfg = CONFIG.lock().await;
            (cfg.wifi_ssid.clone(), cfg.wifi_password.clone())
        };

        if ssid.is_empty() {
            // Scanning starts the WiFi driver. With BLE/WiFi coexistence the
            // BLE radio does not transmit until the WiFi driver is running, so
            // this is required for provisioning to be reachable at all. The
            // list is also handy for the user.
            // Observed on the C6 with esp-radio 1.0.0-beta.1: BLE stops
            // transmitting whenever the WiFi driver is idle. Keep the driver
            // busy with a scan every 20 s until we have credentials.
            info!("wifi: no credentials configured; scanning periodically so coex + BLE work");
            loop {
                match controller.scan_async(&ScanConfig::default().with_max(12)).await {
                    Ok(aps) => {
                        for ap in aps {
                            info!("wifi: seen {:?} rssi {} {:?}", ap.ssid, ap.signal_strength, ap.auth_method);
                        }
                    }
                    Err(e) => warn!("wifi: scan failed: {e:?}"),
                }
                if let Either::Second(()) = embassy_futures::select::select(
                    Timer::after(Duration::from_secs(20)),
                    WIFI_RECONNECT.wait(),
                )
                .await
                {
                    break;
                }
            }
            continue;
        }

        let auth = if password.is_empty() {
            AuthenticationMethodConfig::Open
        } else {
            match password.as_str().try_into() {
                Ok(p) => AuthenticationMethodConfig::Wpa2Personal(p),
                Err(_) => {
                    warn!("wifi: password too long");
                    WIFI_RECONNECT.wait().await;
                    continue;
                }
            }
        };
        let Ok(ssid_cfg) = ssid.as_str().try_into() else {
            warn!("wifi: ssid too long");
            WIFI_RECONNECT.wait().await;
            continue;
        };
        let sta = StationConfig::default()
            .with_ssid(ssid_cfg)
            .with_authentication(auth);
        if let Err(e) = controller.set_config(&Config::Station(sta)) {
            warn!("wifi: set_config failed: {e:?}");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        info!("wifi: connecting to '{}'", ssid);
        match controller.connect_async().await {
            Ok(connected) => {
                info!("wifi: connected to {:?} ch{}", connected.bssid, connected.channel);
                if let Err(e) = controller.set_power_saving(PowerSaveMode::None) {
                    warn!("wifi: power save not applied: {e:?}");
                }
                // Wait for an IP and publish it.
                stack.wait_config_up().await;
                let ip = stack.config_v4().map(|c| c.address.address());
                info!("wifi: got ip {:?}", ip);
                net.send(NetStatus {
                    connected: true,
                    ip,
                    rssi: controller.rssi().ok(),
                });

                // Stay here until we drop off, are asked to reconnect, or it is
                // time to refresh RSSI.
                let reason = loop {
                    match select3(
                        controller.wait_for_disconnect_async(),
                        WIFI_RECONNECT.wait(),
                        Timer::after(Duration::from_secs(30)),
                    )
                    .await
                    {
                        Either3::First(info) => break Reason::Lost(info.is_ok()),
                        Either3::Second(()) => break Reason::Reconnect,
                        Either3::Third(()) => {
                            let status = NetStatus {
                                connected: true,
                                ip: stack.config_v4().map(|c| c.address.address()),
                                rssi: controller.rssi().ok(),
                            };
                            info!(
                                "wifi: up, ip {:?}, rssi {:?}, link {}",
                                status.ip,
                                status.rssi,
                                stack.is_link_up()
                            );
                            net.send(status);
                        }
                    }
                };
                net.send(NetStatus::default());
                match reason {
                    Reason::Lost(clean) => {
                        warn!("wifi: disconnected (clean event: {clean}), retrying in 5 s");
                        Timer::after(Duration::from_secs(5)).await;
                    }
                    Reason::Reconnect => {
                        info!("wifi: reconnect requested");
                        let _ = controller.disconnect_async().await;
                    }
                }
            }
            Err(e) => {
                warn!("wifi: connect failed: {e:?}; retrying in 10 s");
                // A reconnect request (new credentials) cuts the wait short.
                let _ = embassy_futures::select::select(
                    Timer::after(Duration::from_secs(10)),
                    WIFI_RECONNECT.wait(),
                )
                .await;
            }
        }
    }
}

enum Reason {
    Lost(bool),
    Reconnect,
}
