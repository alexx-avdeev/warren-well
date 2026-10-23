//! Sampler task: reads the sensor, filters, converts to a tank level, runs the
//! state machine and publishes `LevelSnapshot`s.
//!
//! With the `sim-sensor` feature (default until the A02YYUW arrives) the raw
//! frames come from a simulator that still goes through the real parser.

use embassy_time::{Duration, Instant, Timer};
use log::{debug, info, warn};
use tank_core::level::a02yyuw::{FrameError, Parser};
use tank_core::level::filter::Median;
use tank_core::level::{Event, StateMachine};

use crate::app::state::{CONFIG, EVENTS, LEVEL, LevelSnapshot, uptime_s};

/// How long to listen for frames per sample before declaring a timeout.
const BURST_WINDOW: Duration = Duration::from_millis(1200);
/// Frames to collect per sample (sensor emits ~3-10 per second).
const BURST_FRAMES: usize = 5;

/// A source of raw sensor bytes.
pub trait ByteSource {
    /// Read some bytes into `buf`, returning how many. May return 0 on timeout.
    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> usize;
}

// ---------------------------------------------------------------------------
// Real sensor on UART1
// ---------------------------------------------------------------------------

#[cfg_attr(feature = "sim-sensor", allow(dead_code))]
pub struct UartSource {
    rx: esp_hal::uart::UartRx<'static, esp_hal::Async>,
}

#[cfg_attr(feature = "sim-sensor", allow(dead_code))]
impl UartSource {
    pub fn new(rx: esp_hal::uart::UartRx<'static, esp_hal::Async>) -> Self {
        Self { rx }
    }
}

impl ByteSource for UartSource {
    async fn read(&mut self, buf: &mut [u8], timeout: Duration) -> usize {
        match embassy_time::with_timeout(timeout, self.rx.read_async(buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                debug!("uart rx error: {e:?}");
                0
            }
            Err(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Simulated sensor
// ---------------------------------------------------------------------------

/// Simulates a slowly draining tank with sensor noise, an occasional wild
/// echo, and a refill when nearly empty. Emits real A02YYUW frames.
pub struct SimSource {
    percent: f32,
    rng: u32,
    pending: heapless::Vec<u8, 32>,
}

impl SimSource {
    pub fn new(seed: u32) -> Self {
        Self {
            percent: 82.0,
            rng: seed | 1,
            pending: heapless::Vec::new(),
        }
    }

    fn next_rand(&mut self) -> u32 {
        // xorshift32
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    fn step(&mut self) -> u16 {
        let (empty_mm, full_mm) = {
            // Can't await here; use try_lock and fall back to defaults.
            match CONFIG.try_lock() {
                Ok(c) => (c.tank.empty_mm, c.tank.full_mm),
                Err(_) => (650, 50),
            }
        };
        // Drain ~0.6 %/min at 1 sample per 10 s → 0.1 %/sample; make it visible: 0.5 %/sample.
        self.percent -= 0.5;
        if self.percent < 6.0 {
            info!("sim: refilling tank");
            self.percent = 95.0;
        }
        let span = empty_mm.saturating_sub(full_mm) as f32;
        let ideal = empty_mm as f32 - span * self.percent / 100.0;
        let noise = (self.next_rand() % 7) as f32 - 3.0; // ±3 mm
        let mut mm = (ideal + noise).max(0.0) as u16;
        if self.next_rand().is_multiple_of(23) {
            mm = 3000 + (self.next_rand() % 500) as u16; // wall echo outlier
        }
        mm
    }
}

impl ByteSource for SimSource {
    async fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> usize {
        if self.pending.is_empty() {
            // Emit a burst: one frame every ~120 ms like the real sensor.
            Timer::after(Duration::from_millis(120)).await;
            let mm = self.step();
            let frame = tank_core::level::a02yyuw::encode(mm);
            // Occasionally corrupt a byte to exercise the checksum path.
            let mut frame = frame;
            if self.next_rand().is_multiple_of(41) {
                frame[1] ^= 0x10;
            }
            let _ = self.pending.extend_from_slice(&frame);
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        // Drain consumed bytes.
        let rest: heapless::Vec<u8, 32> = heapless::Vec::from_slice(&self.pending[n..]).unwrap();
        self.pending = rest;
        n
    }
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

/// Collect a short burst of frames and return their median distance.
async fn read_burst<S: ByteSource>(src: &mut S, parser: &mut Parser) -> Result<u16, SampleError> {
    let deadline = Instant::now() + BURST_WINDOW;
    let mut burst = Median::<BURST_FRAMES>::new();
    let mut last_err: Option<FrameError> = None;
    let mut buf = [0u8; 16];

    while Instant::now() < deadline && burst.len() < BURST_FRAMES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let n = src.read(&mut buf, remaining).await;
        if n == 0 {
            continue;
        }
        parser.push_slice(&buf[..n], |r| match r {
            Ok(mm) => {
                burst.push(mm);
            }
            Err(e) => last_err = Some(e),
        });
    }

    match burst.median() {
        Some(mm) => Ok(mm),
        None => Err(match last_err {
            Some(e) => SampleError::Frame(e),
            None => SampleError::Timeout,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SampleError {
    Timeout,
    Frame(#[allow(dead_code)] FrameError),
}

async fn run<S: ByteSource>(mut src: S) -> ! {
    let publish = LEVEL.sender();
    let mut parser = Parser::new();
    let mut smooth = Median::<5>::new();
    let mut sm = StateMachine::new();
    let mut sample_no: u32 = 0;
    let mut snapshot = LevelSnapshot::default();

    info!("sampler: started");
    loop {
        let (tank, thresholds, interval) = {
            let cfg = CONFIG.lock().await;
            (
                cfg.tank,
                cfg.thresholds(),
                Duration::from_secs(cfg.sample_interval_s as u64),
            )
        };
        let started = Instant::now();

        sample_no = sample_no.wrapping_add(1);
        let mut event: Option<Event> = None;

        match read_burst(&mut src, &mut parser).await {
            Ok(raw_mm) => {
                let mm = smooth.push(raw_mm).unwrap_or(raw_mm);
                let level = tank.level(mm);
                if tank.is_calibrated() {
                    event = sm.update(level.percent, &thresholds);
                }
                snapshot = LevelSnapshot {
                    raw_mm: Some(mm),
                    level: Some(level),
                    state: sm.state(),
                    last_event: event.or(snapshot.last_event),
                    sample_no,
                    uptime_s: uptime_s(),
                };
                // Heartbeat at info level once a minute; every sample at debug.
                if sample_no % 6 == 1 {
                    info!(
                        "level: {mm} mm, depth {} mm, {:.1} %, {:.1} L, {:?} (sample #{sample_no})",
                        level.depth_mm, level.percent, level.litres, sm.state()
                    );
                } else {
                    debug!(
                        "sample #{sample_no}: raw {raw_mm} mm → {mm} mm, depth {} mm, {:.1} %, {:.1} L, {:?}",
                        level.depth_mm, level.percent, level.litres, sm.state()
                    );
                }
            }
            Err(e) => {
                event = sm.invalid(&thresholds);
                snapshot = LevelSnapshot {
                    raw_mm: snapshot.raw_mm,
                    level: snapshot.level,
                    state: sm.state(),
                    last_event: event.or(snapshot.last_event),
                    sample_no,
                    uptime_s: uptime_s(),
                };
                warn!("sample #{sample_no}: no valid reading ({e:?}), state {:?}", sm.state());
            }
        }

        publish.send(snapshot);
        if let Some(ev) = event {
            info!("level event: {ev:?}");
            let _ = EVENTS.try_send(ev);
        }

        let elapsed = started.elapsed();
        if elapsed < interval {
            Timer::after(interval - elapsed).await;
        }
    }
}

#[cfg(feature = "sim-sensor")]
#[embassy_executor::task]
pub async fn level_task(seed: u32) -> ! {
    info!("sampler: using SIMULATED sensor (feature sim-sensor)");
    run(SimSource::new(seed)).await
}

#[cfg(not(feature = "sim-sensor"))]
#[embassy_executor::task]
pub async fn level_task(rx: esp_hal::uart::UartRx<'static, esp_hal::Async>) -> ! {
    info!("sampler: using A02YYUW on UART1");
    run(UartSource::new(rx)).await
}
