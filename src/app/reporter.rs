//! Builds the JSON report and hands it to a [`Reporter`].
//!
//! Only [`LogReporter`] exists for now: it prints the exact body that will
//! later be POSTed to the Rails endpoint. Swapping in an HTTP reporter is a
//! one-line change in `main.rs`.

use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Timer};
use heapless::String;
use log::{info, warn};
use tank_core::FIRMWARE_VERSION;
use tank_core::payload::{LevelReport, Report};

use crate::app::state::{CONFIG, DEVICE_ID, EVENTS, LEVEL, NET, REPORT_NOW, uptime_s};

pub const REPORT_BUF: usize = 512;

pub trait Reporter {
    /// Deliver one report body. `url` is the configured endpoint (may be empty).
    async fn send(&mut self, url: &str, token: &str, json: &str) -> Result<(), ()>;
}

/// Stub transport: logs the report instead of sending it.
pub struct LogReporter;

impl Reporter for LogReporter {
    async fn send(&mut self, url: &str, _token: &str, json: &str) -> Result<(), ()> {
        if url.is_empty() {
            info!("report (no server configured): {json}");
        } else {
            info!("report (stub, would POST to {url}): {json}");
        }
        Ok(())
    }
}

#[embassy_executor::task]
pub async fn reporter_task(mut reporter: LogReporter) -> ! {
    let mut seq: u32 = 0;
    let mut level_rx = LEVEL.receiver().unwrap();
    info!("reporter: started");

    loop {
        let interval = {
            let cfg = CONFIG.lock().await;
            Duration::from_secs(cfg.report_interval_s as u64)
        };

        // Wait for: periodic tick, a level event, or an explicit request.
        let event = match select3(Timer::after(interval), EVENTS.receive(), REPORT_NOW.wait()).await {
            Either3::First(()) => {
                info!("reporter: periodic report");
                None
            }
            Either3::Second(ev) => {
                info!("reporter: event report ({ev:?})");
                Some(ev)
            }
            Either3::Third(()) => {
                info!("reporter: requested report");
                None
            }
        };

        let snapshot = level_rx.try_get().unwrap_or_default();
        let net = NET.try_get().unwrap_or_default();
        let device_id = DEVICE_ID.lock().await.clone();
        let (url, token) = {
            let cfg = CONFIG.lock().await;
            (cfg.api_url.clone(), cfg.api_token.clone())
        };

        seq = seq.wrapping_add(1);
        let report = Report {
            device_id: &device_id,
            seq,
            fw: FIRMWARE_VERSION,
            uptime_s: uptime_s(),
            rssi: net.rssi,
            level: LevelReport::from_level(snapshot.level, snapshot.state, event),
            sampled_at_uptime_s: snapshot.uptime_s,
        };

        let mut json: String<REPORT_BUF> = String::new();
        match serde_json_core::to_string::<_, REPORT_BUF>(&report) {
            Ok(s) => {
                let _ = json.push_str(&s);
            }
            Err(e) => {
                warn!("report: serialise failed: {e:?}");
                continue;
            }
        }

        if reporter.send(&url, &token, &json).await.is_err() {
            warn!("report: delivery failed (seq {seq})");
        }
    }
}
