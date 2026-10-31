//! Control loop: executes `Command`s from the UIs (save, calibrate, reboot…).

use embassy_time::{Duration, Timer};
use log::{info, warn};
use tank_core::config::DeviceConfig;

use crate::app::state::{BLE_WINDOW, COMMANDS, CONFIG, Command, LEVEL, REPORT_NOW, WIFI_RECONNECT};
use crate::app::storage::ConfigStore;

pub async fn run(mut store: ConfigStore) -> ! {
    loop {
        let cmd = COMMANDS.receive().await;
        info!("command: {cmd:?}");
        match cmd {
            Command::SaveConfig { reconnect_wifi } => {
                let cfg = CONFIG.lock().await.clone();
                store.save(&cfg).await;
                if reconnect_wifi {
                    WIFI_RECONNECT.signal(());
                }
            }
            Command::Reboot => reboot().await,
            Command::CalibrateFull | Command::CalibrateEmpty => {
                let Some(mm) = LEVEL.try_get().and_then(|s| s.raw_mm) else {
                    warn!("calibrate: no sensor reading available");
                    continue;
                };
                let cfg = {
                    let mut cfg = CONFIG.lock().await;
                    if cmd == Command::CalibrateFull {
                        cfg.tank.full_mm = mm;
                    } else {
                        cfg.tank.empty_mm = mm;
                    }
                    info!(
                        "calibrate: full={} mm empty={} mm",
                        cfg.tank.full_mm, cfg.tank.empty_mm
                    );
                    cfg.clone()
                };
                store.save(&cfg).await;
            }
            Command::FactoryReset => {
                let cfg = DeviceConfig::default();
                *CONFIG.lock().await = cfg.clone();
                store.save(&cfg).await;
                reboot().await;
            }
            Command::TestReport => REPORT_NOW.signal(()),
            Command::OpenBleWindow => BLE_WINDOW.signal(()),
            Command::Enroll => {
                let cfg = {
                    let mut cfg = CONFIG.lock().await;
                    cfg.api_token.clear();
                    cfg.clone()
                };
                store.save(&cfg).await;
                info!("enrol: token cleared, re-enrolling");
                REPORT_NOW.signal(());
            }
        }
    }
}

async fn reboot() -> ! {
    info!("rebooting…");
    Timer::after(Duration::from_millis(300)).await;
    esp_hal::system::software_reset()
}
