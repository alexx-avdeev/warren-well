//! Talks to the server (protocol v1.1, see `docs/PROTOCOL.md`):
//!
//! * enrols itself when it has a server URL but no token (`POST /devices/enroll`),
//! * POSTs level reports (`POST /tank_readings`), queued while offline and
//!   replayed byte-for-byte so the server can deduplicate on `(device, boot_id, seq)`,
//! * applies `server_time` and the versioned `config` block from replies,
//! * handles 403 pending approval (wait), 401 (three in a row → re-enrol), 429.

use core::sync::atomic::Ordering;

use embassy_futures::select::{Either3, select3};
use embassy_net::Stack;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use heapless::{Deque, String};
use log::{info, warn};
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::{Method, RequestBuilder};
use tank_core::FIRMWARE_VERSION;
use tank_core::config::{EnrolReply, EnrolRequest, ServerReply};
use tank_core::payload::{LevelReport, Report};

use crate::app::state::{
    BOOT_ID, COMMANDS, CONFIG, Command, DEVICE_ID, EVENTS, LEVEL, NET, REPORT, REPORT_NOW,
    ReportError, ReportStatus, TIME_OFFSET, display_name, unix_now, uptime_s,
};
use crate::mk_static;

pub const REPORT_BUF: usize = 512;
/// Newest reports kept while offline; older ones are discarded.
const QUEUE_LEN: usize = 16;
const RX_BUF: usize = 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const PENDING_RETRY: Duration = Duration::from_secs(300);
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(60);
/// Consecutive 401s before the token is considered dead.
const UNAUTHORIZED_LIMIT: u8 = 3;
const HARDWARE: &str = "esp32c6-devkitc-1";
const DEVICE_KIND: &str = "water_tank";

type Body = String<REPORT_BUF>;
type UrlBuf = String<160>;

/// Outcome of one HTTP exchange: status code and (truncated) body text.
struct Exchange {
    code: u16,
    body: String<RX_BUF>,
}

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

    /// POST a JSON body, optionally with a bearer token.
    async fn post_json(&mut self, url: &str, token: Option<&str>, body: &str) -> Result<Exchange, ReportError> {
        let mut auth: String<80> = String::new();
        let _ = auth.push_str("Bearer ");
        let _ = auth.push_str(token.unwrap_or(""));
        let with_auth = [("Authorization", auth.as_str()), ("Connection", "close")];
        let without_auth = [("Connection", "close")];

        let handle = match with_timeout(HTTP_TIMEOUT, self.client.request(Method::POST, url)).await {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                warn!("http: connect to {url} failed: {e:?}");
                return Err(ReportError::Connect);
            }
            Err(_) => return Err(ReportError::Timeout),
        };
        let headers: &[(&str, &str)] = if token.is_some() { &with_auth } else { &without_auth };
        let mut handle = handle
            .headers(headers)
            .content_type(ContentType::ApplicationJson)
            .body(body.as_bytes());

        let response = match with_timeout(HTTP_TIMEOUT, handle.send(&mut self.rx)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!("http: request failed: {e:?}");
                return Err(ReportError::Connect);
            }
            Err(_) => return Err(ReportError::Timeout),
        };
        let code = response.status.0;
        let mut text: String<RX_BUF> = String::new();
        match with_timeout(HTTP_TIMEOUT, response.body().read_to_end()).await {
            Ok(Ok(b)) => {
                let s = core::str::from_utf8(b).unwrap_or("");
                let _ = text.push_str(&s[..s.len().min(RX_BUF)]);
            }
            Ok(Err(e)) => warn!("http: body read failed: {e:?}"),
            Err(_) => return Err(ReportError::Timeout),
        }
        Ok(Exchange { code, body: text })
    }
}

fn url(base: &str, path: &str) -> UrlBuf {
    let mut u = UrlBuf::new();
    let _ = u.push_str(base);
    let _ = u.push_str(path);
    u
}

/// Pull `retry_s` out of a reply body, if present.
fn retry_hint(body: &str) -> Option<Duration> {
    serde_json_core::from_str::<EnrolReply>(body)
        .ok()
        .and_then(|(r, _)| r.retry_s)
        .map(|s| Duration::from_secs(s.clamp(5, 3600) as u64))
}

/// Apply what the server told us in a 2xx reply to a report.
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
            info!("report: applied server config version {:?}", cfg.config_version);
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

/// Ask the server for a token. On success the token is stored and persisted.
async fn enrol(http: &mut HttpReporter, base: &str) -> Result<(), (ReportError, Option<Duration>)> {
    let device_id = DEVICE_ID.lock().await.clone();
    let name = display_name().await;
    let req = EnrolRequest {
        device_id: &device_id,
        kind: DEVICE_KIND,
        fw: FIRMWARE_VERSION,
        name: &name,
        hw: HARDWARE,
        simulated: cfg!(feature = "sim-sensor"),
        boot_id: BOOT_ID.load(Ordering::Relaxed),
    };
    let body = serde_json_core::to_string::<_, 256>(&req).map_err(|_| (ReportError::EnrolFailed, None))?;
    let u = url(base, "/devices/enroll");
    info!("enrol: requesting token from {u}");
    let ex = http.post_json(&u, None, &body).await.map_err(|e| (e, None))?;
    match ex.code {
        200..=299 => {
            let reply = serde_json_core::from_str::<EnrolReply>(&ex.body)
                .map(|(r, _)| r)
                .map_err(|_| (ReportError::EnrolFailed, None))?;
            let Some(token) = reply.api_token else {
                warn!("enrol: reply without api_token: {}", ex.body);
                return Err((ReportError::EnrolFailed, None));
            };
            info!(
                "enrol: got {} token ({} chars); waiting for approval",
                reply.status.as_deref().unwrap_or("pending"),
                token.len()
            );
            CONFIG.lock().await.api_token = token;
            let _ = COMMANDS.try_send(Command::SaveConfig {
                reconnect_wifi: false,
            });
            Ok(())
        }
        429 => Err((ReportError::RateLimited, retry_hint(&ex.body))),
        500..=599 => {
            warn!("enrol: server error {}: {}", ex.code, ex.body);
            Err((ReportError::ServerError(ex.code), None))
        }
        code => {
            warn!("enrol: rejected {code}: {}", ex.body);
            Err((ReportError::EnrolFailed, None))
        }
    }
}

/// Outcome of posting one queued report.
enum Posted {
    Ok,
    /// Drop the report, it will never be accepted.
    Drop(u16),
    /// Keep the report; wait `Duration` before trying again.
    Wait(ReportError, Duration),
}

async fn post_report(http: &mut HttpReporter, base: &str, token: &str, body: &str, backoff: Duration) -> Posted {
    let u = url(base, "/tank_readings");
    match http.post_json(&u, Some(token), body).await {
        Err(e) => Posted::Wait(e, backoff),
        Ok(ex) => match ex.code {
            200..=299 => {
                let reply = serde_json_core::from_str::<ServerReply>(&ex.body)
                    .map(|(r, _)| r)
                    .unwrap_or_else(|e| {
                        warn!("report: reply not understood ({e:?}): {}", ex.body);
                        ServerReply::default()
                    });
                apply_reply(&reply).await;
                Posted::Ok
            }
            401 => {
                warn!("report: unauthorized: {}", ex.body);
                Posted::Wait(ReportError::Unauthorized, UNAUTHORIZED_RETRY)
            }
            403 => {
                info!("report: device not approved yet on the server");
                Posted::Wait(ReportError::PendingApproval, PENDING_RETRY)
            }
            429 => Posted::Wait(ReportError::RateLimited, retry_hint(&ex.body).unwrap_or(backoff)),
            400..=499 => {
                warn!("report: rejected ({}): {} -> body was {body}", ex.code, ex.body);
                Posted::Drop(ex.code)
            }
            code => {
                warn!("report: server error ({code}): {}", ex.body);
                Posted::Wait(ReportError::ServerError(code), backoff)
            }
        },
    }
}

#[embassy_executor::task]
pub async fn reporter_task(mut http: HttpReporter) -> ! {
    let mut seq: u32 = 0;
    let mut queue: Deque<Body, QUEUE_LEN> = Deque::new();
    let mut status = ReportStatus::default();
    let mut backoff = BACKOFF_MIN;
    let mut unauthorized_streak: u8 = 0;
    // When the last attempt failed, the time at which to try again. Starts
    // a few seconds after boot so a fresh device enrols and shows up on the
    // server right away instead of after the first report interval.
    let mut retry_at: Option<Instant> = Some(Instant::now() + Duration::from_secs(5));
    let publish = REPORT.sender();
    publish.send(status);
    info!("reporter: started");

    loop {
        let interval = {
            let cfg = CONFIG.lock().await;
            Duration::from_secs(cfg.report_interval_s as u64)
        };
        let wake = match retry_at {
            Some(at) => Timer::at(at.min(Instant::now() + interval)),
            None => Timer::after(interval),
        };
        let (event, new_report) = match select3(wake, EVENTS.receive(), REPORT_NOW.wait()).await {
            Either3::First(()) => {
                // A retry wake-up only replays the queue; a periodic tick (or the
                // boot-time first report) adds a new one.
                let retry_only = retry_at.is_some_and(|at| Instant::now() >= at) && !queue.is_empty();
                (None, !retry_only)
            }
            Either3::Second(ev) => {
                info!("reporter: event {ev:?}");
                (Some(ev), true)
            }
            Either3::Third(()) => {
                info!("reporter: report requested");
                (None, true)
            }
        };
        retry_at = None;

        if new_report {
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

        let (base, token) = {
            let cfg = CONFIG.lock().await;
            let mut b: UrlBuf = UrlBuf::new();
            let _ = b.push_str(cfg.api_base());
            (b, cfg.api_token.clone())
        };
        status.enrolled = !token.is_empty();
        status.queued = queue.len() as u8;

        if base.is_empty() {
            if let Some(b) = queue.back() {
                info!("report (no server configured): {b}");
            }
            queue.clear();
            status.queued = 0;
            status.last_error = Some(ReportError::NoServer);
            publish.send(status);
            continue;
        }
        if !NET.try_get().unwrap_or_default().connected {
            status.last_error = Some(ReportError::Offline);
            publish.send(status);
            retry_at = Some(Instant::now() + BACKOFF_MIN);
            continue;
        }

        // No token yet (fresh device, factory reset, or token declared dead): enrol first.
        let token = if token.is_empty() {
            match enrol(&mut http, &base).await {
                Ok(()) => {
                    unauthorized_streak = 0;
                    status.enrolled = true;
                    status.last_error = Some(ReportError::PendingApproval);
                    CONFIG.lock().await.api_token.clone()
                }
                Err((e, hint)) => {
                    status.last_error = Some(e);
                    publish.send(status);
                    let wait = hint.unwrap_or(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    warn!("enrol failed ({e:?}); retry in {} s", wait.as_secs());
                    retry_at = Some(Instant::now() + wait);
                    continue;
                }
            }
        } else {
            token
        };

        // Drain the queue oldest-first until something asks us to wait.
        while let Some(body) = queue.front() {
            match post_report(&mut http, &base, &token, body, backoff).await {
                Posted::Ok => {
                    info!("report sent ({} B) -> ok", body.len());
                    queue.pop_front();
                    status.sent = status.sent.wrapping_add(1);
                    status.last_ok_uptime_s = Some(uptime_s());
                    status.last_error = None;
                    backoff = BACKOFF_MIN;
                    unauthorized_streak = 0;
                }
                Posted::Drop(code) => {
                    warn!("report dropped (HTTP {code})");
                    queue.pop_front();
                    status.last_error = Some(ReportError::Rejected(code));
                }
                Posted::Wait(e, wait) => {
                    status.last_error = Some(e);
                    if e == ReportError::Unauthorized {
                        unauthorized_streak += 1;
                        if unauthorized_streak >= UNAUTHORIZED_LIMIT {
                            warn!("report: {unauthorized_streak} consecutive 401s, forgetting token and re-enrolling");
                            CONFIG.lock().await.api_token.clear();
                            let _ = COMMANDS.try_send(Command::SaveConfig {
                                reconnect_wifi: false,
                            });
                            unauthorized_streak = 0;
                            retry_at = Some(Instant::now() + BACKOFF_MIN);
                            break;
                        }
                    } else if matches!(e, ReportError::Connect | ReportError::Timeout | ReportError::ServerError(_)) {
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                    warn!("report deferred ({e:?}); {} queued, retry in {} s", queue.len(), wait.as_secs());
                    retry_at = Some(Instant::now() + wait);
                    break;
                }
            }
        }
        status.queued = queue.len() as u8;
        publish.send(status);
    }
}
