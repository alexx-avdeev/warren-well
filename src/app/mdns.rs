//! mDNS: answer for `tank-xxxx.local` and advertise the web UI as
//! `_http._tcp`, and browse for the farm server's `_farmos._tcp` service
//! when no server URL is configured. The codec is in `tank_core::mdns`; this
//! task only owns the UDP socket and the timers.

use embassy_futures::select::{select4, Either4};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_time::{Duration, Instant, Timer};
use heapless::String;
use log::{debug, info, warn};
use tank_core::mdns::{self, Browser, Host, MDNS_GROUP, MDNS_PORT};
use tank_core::FIRMWARE_VERSION;

use crate::app::state::{mdns_hostname, Discovered, CONFIG, DEVICE_ID, DISCOVERED, NET, REDISCOVER, REPORT_NOW};

/// DNS-SD service type the farm app advertises (Avahi on the Rails host).
pub const FARM_SERVICE: &str = "_farmos._tcp";
const HTTP_PORT: u16 = 80;
/// Browse cadence while nothing is found / once found (re-check for a move).
const BROWSE_FAST: Duration = Duration::from_secs(5);
const BROWSE_SLOW: Duration = Duration::from_secs(300);
const RX_BUF: usize = 1024;
const TX_BUF: usize = 512;

fn group() -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from_octets(MDNS_GROUP)), MDNS_PORT)
}

#[embassy_executor::task]
pub async fn mdns_task(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; RX_BUF];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0u8; TX_BUF];
    let mut pkt = [0u8; 600];
    let mut out = [0u8; TX_BUF];

    let hostname = mdns_hostname().await;
    let id_txt: String<16> = {
        let mut s = String::new();
        let _ = s.push_str("id=");
        let _ = s.push_str(&DEVICE_ID.lock().await);
        s
    };
    let fw_txt: String<24> = {
        let mut s = String::new();
        let _ = s.push_str("fw=");
        let _ = s.push_str(FIRMWARE_VERSION);
        s
    };
    let txt = ["path=/", id_txt.as_str(), fw_txt.as_str()];

    let mut net_rx = NET.receiver().expect("NET receiver");
    let found = DISCOVERED.sender();
    found.send(None);
    let mut browser = Browser::new(FARM_SERVICE).expect("service name");
    let mut mcast_joined = false;

    loop {
        // Wait for an IP.
        let ip = loop {
            let s = net_rx.changed().await;
            if let (true, Some(ip)) = (s.connected, s.ip) {
                break ip;
            }
        };
        if !mcast_joined {
            match stack.join_multicast_group(Ipv4Address::from_octets(MDNS_GROUP)) {
                Ok(()) => mcast_joined = true,
                Err(e) => warn!("mdns: join multicast failed: {e:?}"),
            }
        }
        let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
        socket.set_hop_limit(Some(255));
        if let Err(e) = socket.bind(MDNS_PORT) {
            warn!("mdns: bind failed: {e:?}");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }
        let mut host = Host {
            hostname: &hostname,
            ip: ip.octets(),
            http_port: HTTP_PORT,
            txt: &txt,
        };
        info!("mdns: {}.local -> {}", hostname, ip);
        announce(&socket, &host, &mut out).await;
        Timer::after(Duration::from_secs(1)).await;
        announce(&socket, &host, &mut out).await;

        browser.reset();
        let mut next_browse = Instant::now();
        loop {
            match select4(
                socket.recv_from(&mut pkt),
                net_rx.changed(),
                REDISCOVER.wait(),
                Timer::at(next_browse),
            )
            .await
            {
                Either4::First(Ok((n, meta))) => {
                    let from = meta.endpoint;
                    if from.addr == IpAddress::Ipv4(ip) {
                        continue; // our own packet
                    }
                    let packet = &pkt[..n];
                    // Browse replies.
                    if browsing().await {
                        match browser.ingest(packet) {
                            Ok(true) => {
                                if let Some(s) = browser.result() {
                                    let d = Discovered {
                                        ip: Ipv4Address::from_octets(s.ip),
                                        port: s.port,
                                        base: s.base,
                                    };
                                    if DISCOVERED.try_get().flatten().as_ref() != Some(&d) {
                                        info!(
                                            "mdns: farm server '{}' at {}",
                                            browser.instance().map(|i| i.to_dotted::<48>()).unwrap_or_default(),
                                            d.url::<64>()
                                        );
                                        found.send(Some(d));
                                        REPORT_NOW.signal(());
                                    }
                                    next_browse = Instant::now() + BROWSE_SLOW;
                                }
                            }
                            Ok(false) => {}
                            Err(e) => debug!("mdns: unparsable packet from {}: {e:?}", from),
                        }
                    }
                    // Queries for us.
                    match mdns::respond(&host, packet, &mut out) {
                        Ok(Some(reply)) => {
                            // Legacy unicast queries (source port != 5353) and QU
                            // questions get a direct answer; everything else goes
                            // to the group so other caches learn it too.
                            let to = if reply.unicast || from.port != MDNS_PORT { from } else { group() };
                            if let Err(e) = socket.send_to(&out[..reply.len], to).await {
                                warn!("mdns: send failed: {e:?}");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => debug!("mdns: bad query from {}: {e:?}", from),
                    }
                }
                Either4::First(Err(e)) => {
                    warn!("mdns: recv error: {e:?}");
                }
                Either4::Second(status) => {
                    match (status.connected, status.ip) {
                        (true, Some(new_ip)) if new_ip == ip => {}
                        (true, Some(new_ip)) => {
                            info!("mdns: ip changed to {new_ip}, re-announcing");
                            host.ip = new_ip.octets();
                            announce(&socket, &host, &mut out).await;
                        }
                        _ => {
                            info!("mdns: link down");
                            break;
                        }
                    }
                }
                Either4::Third(()) => {
                    info!("mdns: re-discovery requested");
                    browser.reset();
                    next_browse = Instant::now();
                }
                Either4::Fourth(()) => {
                    if browsing().await {
                        if browser.result().is_some() {
                            // Periodic re-check: start over so a moved server is noticed.
                            browser.reset();
                        }
                        match browser.query(&mut out) {
                            Ok(len) => {
                                if let Err(e) = socket.send_to(&out[..len], group()).await {
                                    warn!("mdns: browse send failed: {e:?}");
                                }
                            }
                            Err(e) => warn!("mdns: browse query build failed: {e:?}"),
                        }
                        next_browse = Instant::now() + BROWSE_FAST;
                    } else {
                        next_browse = Instant::now() + Duration::from_secs(60);
                    }
                }
            }
        }
        // Socket dropped here; the IP wait at the top will re-create it.
    }
}

/// Browse only while the user has not configured a server URL.
async fn browsing() -> bool {
    CONFIG.lock().await.api_base().is_empty()
}

async fn announce(socket: &UdpSocket<'_>, host: &Host<'_>, out: &mut [u8]) {
    match mdns::announce(host, out) {
        Ok(len) => {
            if let Err(e) = socket.send_to(&out[..len], group()).await {
                warn!("mdns: announce failed: {e:?}");
            }
        }
        Err(e) => warn!("mdns: announce build failed: {e:?}"),
    }
}
