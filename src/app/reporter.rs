//! Builds the JSON report and POSTs it to the server (protocol v1, see
//! `docs/PROTOCOL.md`). Reports are queued when the server is unreachable
//! and replayed byte-for-byte, so the server can deduplicate on
//! `(device, boot_id, seq)`.

use core::sync::atomic::Ordering;

use embassy_futures::select::{Either3, select3};
use embassy_net::Stack;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_time::{Duration, Timer, with_timeout};
use heapless::{Deque, String};
use log::{info, warn};
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::{Method, RequestBuilder};
use tank_core::FIRMWARE_VERSION;
use tank_core::config::ServerReply;
use tank_core::payload::{LevelReport, Report};

use crate::app::state::{
    BOOT_ID, COMMANDS, CONFIG, Command, DEVICE_ID, EVENTS, LEVEL, NET, REPORT, REPORT_NOW,
    ReportError, ReportStatus, TIME_OFFSET, unix_now, uptime_s,
};
use crate::mk_static;

pub const REPORT_BUF: usize = 512;
/// Newest reports kept while offline; older ones are discarded.
const QUEUE_LEN: usize = 16;
const RX_BUF: usize = 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(300);

type Body = String<REPORT_BUF>;

pub struct HttpReporter {
    client: HttpClient<'static, TcpClient<'static, 1, 1024, 1024>, DnsSocket<'static>>,
    rx: [u8; RX_BUF],
}

impl HttpReporter {
    pub fn new(stack: Stack<'static>) -> Self {
        let tcp_state = mk_static!(TcpClientState<1, 1024, 1024>, TcpClientState::new());
        let tcp = mk_static!(TcpClient<'static, 1, 1024, 1024>, TcpClient::new(stack, tcp_state));
        let dns = mk_static!(DnsSocket<'static>, DnsSocket::new(stack));
        Self {
            client: HttpClient::new(tcp, dns),
            rx: [0; RX_BUF],
        }
    }

    /// POST one serialised report. `Ok(reply)` on 2xx.
    async fn post(&mut self, url: &str, token: &str, body: &str) -> Result<ServerReply, ReportError> {
        let mut auth: String<80> = String::new();
        let _ = auth.push_str("Bearer ");
        let _ = auth.push_str(token);
        let headers = [("Authorization", auth.as_str()), ("Connection", "close")];

        let request = with_timeout(HTTP_TIMEOUT, self.client.request(Method::POST, url)).await;
        let handle = match request {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                warn!("report: connect failed: {e:?}");
                return Err(ReportError::Connect);
            }
            Err(_) => return Err(ReportError::Timeout),
        };
        let mut handle = handle
            .headers(&headers)
            .content_type(ContentType::ApplicationJson)
            .body(body.as_bytes());

        let response = match with_timeout(HTTP_TIMEOUT, handle.send(&mut self.rx)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!("report: request failed: {e:?}");
                return Err(ReportError::Connect);
            }
            Err(_) => return Err(ReportError::Timeout),
        };
        let code = response.status.0;
        let body = match with_timeout(HTTP_TIMEOUT, response.body().read_to_end()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                warn!("report: body read failed: {e:?}");
                &mut [][..]
            }
            Err(_) => return Err(ReportError::Timeout),
        };
        let text = core::str::from_utf8(body).unwrap_or("");
        match code {
            200..=299 => {
                let reply = serde_json_core::from_str::<ServerReply>(text)
                    .map(|(r, _)| r)
                    .unwrap_or_else(|e| {
                        warn!("report: reply not understood ({e:?}): {text}");
                        ServerReply::default()
                    });
                Ok(reply)
            }
            401 | 403 => {
                warn!("report: unauthorized ({code}): {text}");
                Err(ReportError::Unauthorized)
            }
            400..=499 => {
                warn!("report: rejected ({code}): {text}");
                Err(ReportError::Rejected(code))
            }
            _ => {
                warn!("report: server error ({code}): {text}");
                Err(ReportError::ServerError(code))
            }
        }
    }
}

/// Apply what the server told us in a 2xx reply.
async fn apply_reply(reply: &ServerReply) {
    if let Some(t) = reply.server_time {
        let offset = t.saturating_sub(uptime_s()) as u32;
        if TIME_OFFSET.swap(offset, Ordering::Relaxed) == 0 {
            info!("report: time synced from server (unix {t})");
        }
    }
    if let Some(cfg) = &reply.config {
        let applied = cfg.apply_if_newer(&mut *CONFIG.lock().await);
        if applied {
            info!(
                "report: applied server config version {:?}",
                cfg.config_version
            );
            let _ = COMMANDS.try_send(Command::SaveConfig {
                reconnect_wifi: false,
            });
        }
    }
}

fn build_report(seq: u32, event: Option<tank_core::level::Event>, device_id: &str) -> Option<Body> {
    let snapshot = LEVEL.try_get().unwrap_or_default();
    let net = NET.try_get().unwrap_or_default();
    let sampled_at = unix_now().map(|now| now - (uptime_s() - snapshot.uptime_s));
    let report = Report {
        device_id,
        boot_id: BOOT_ID.load(Ordering::Relaxed),
        seq,
        fw: FIRMWARE_VERSION,
        uptime_s: uptime_s(),
        rssi: net.rssi,
        level: LevelReport::from_level(snapshot.level, snapshot.state, event),
        sampled_at_uptime_s: snapshot.uptime_s,
        sampled_at,
    };
    match serde_json_core::to_string::<_, REPORT_BUF>(&report) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("report: serialise failed: {e:?}");
            None
        }
    }
}

#[embassy_executor::task]
pub async fn reporter_task(mut http: HttpReporter) -> ! {
    let mut seq: u32 = 0;
    let mut queue: Deque<Body, QUEUE_LEN> = Deque::new();
    let mut status = ReportStatus::default();
    let mut backoff = BACKOFF_MIN;
    // Set when the last attempt failed; the next attempt waits for it.
    let mut retry_at: Option<embassy_time::Instant> = None;
    let publish = REPORT.sender();
    publish.send(status);
    info!("reporter: started");

    loop {
        let interval = {
            let cfg = CONFIG.lock().await;
            Duration::from_secs(cfg.report_interval_s as u64)
        };
        // Wake on: periodic tick, a level event, an explicit request, or the retry timer.
        let next_tick = match retry_at {
            Some(at) if !queue.is_empty() => Timer::at(at.min(embassy_time::Instant::now() + interval)),
            _ => Timer::after(interval),
        };
        let event = match select3(next_tick, EVENTS.receive(), REPORT_NOW.wait()).await {
            Either3::First(()) => None,
            Either3::Second(ev) => {
                info!("reporter: event {ev:?}");
                Some(ev)
            }
            Either3::Third(()) => {
                info!("reporter: report requested");
                None
            }
        };

        // Build a new report unless this wake-up was only a retry of queued ones.
        let is_retry_only = event.is_none() && retry_at.is_some_and(|at| embassy_time::Instant::now() >= at)
            && !queue.is_empty()
            && status.last_error.is_some();
        if !is_retry_only || queue.is_empty() {
            seq = seq.wrapping_add(1);
            let device_id = DEVICE_ID.lock().await.clone();
            if let Some(body) = build_report(seq, event, &device_id) {
                if queue.is_full() {
                    let _ = queue.pop_front();
                    warn!("reporter: queue full, dropping oldest report");
                }
                let _ = queue.push_back(body);
            }
        }

        let (url, token) = {
            let cfg = CONFIG.lock().await;
            (cfg.api_url.clone(), cfg.api_token.clone())
        };
        if url.is_empty() {
            if let Some(b) = queue.back() {
                info!("report (no server configured): {b}");
            }
            queue.clear();
            status.last_error = Some(ReportError::NoServer);
            status.queued = 0;
            publish.send(status);
            continue;
        }
        if !NET.try_get().unwrap_or_default().connected {
            status.last_error = Some(ReportError::Offline);
            status.queued = queue.len() as u8;
            publish.send(status);
            retry_at = Some(embassy_time::Instant::now() + BACKOFF_MIN);
            continue;
        }

        // Drain the queue oldest-first until something fails.
        while let Some(body) = queue.front() {
            match http.post(&url, &token, body).await {
                Ok(reply) => {
                    info!("report sent ({} B, seq in body) -> ok", body.len());
                    queue.pop_front();
                    status.sent = status.sent.wrapping_add(1);
                    status.last_ok_uptime_s = Some(uptime_s());
                    status.last_error = None;
                    backoff = BACKOFF_MIN;
                    retry_at = None;
                    apply_reply(&reply).await;
                }
                Err(ReportError::Rejected(code)) => {
                    warn!("report dropped (HTTP {code})");
                    queue.pop_front();
                    status.last_error = Some(ReportError::Rejected(code));
                }
                Err(ReportError::Unauthorized) => {
                    status.last_error = Some(ReportError::Unauthorized);
                    retry_at = Some(embassy_time::Instant::now() + UNAUTHORIZED_RETRY);
                    break;
                }
                Err(e) => {
                    status.last_error = Some(e);
                    retry_at = Some(embassy_time::Instant::now() + backoff);
                    warn!("report failed ({e:?}); {} queued, retry in {} s", queue.len(), backoff.as_secs());
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    break;
                }
            }
        }
        status.queued = queue.len() as u8;
        publish.send(status);
    }
}
