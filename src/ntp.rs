//! NTP Time Synchronization
//!
//! Provides time synchronization via NTP with a 12-hour sync interval
//! (30-second retry interval after a failed sync).
//! Updates unix time via set_unix_time() for synchronous access.

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use sntpc::{NtpContext, NtpTimestampGenerator};

use crate::set_unix_time;

// NTP configuration - Cloudflare NTP servers (no DNS lookup needed)
const NTP_SERVER_IP: [u8; 4] = [162, 159, 200, 1];
const NTP_PORT: u16 = 123;

// Timeouts
const NTP_TIMEOUT_SECS: u64 = 5;

// Sync interval: 12 hours
const SYNC_INTERVAL_SECS: u64 = 12 * 60 * 60;

// Retry interval after a failed sync: 30 seconds
const RETRY_INTERVAL_SECS: u64 = 30;

/// Timestamp generator for sntpc - uses embassy_time::Instant
#[derive(Clone, Copy)]
struct EmbassyTimestamp {
    start: Instant,
}

impl EmbassyTimestamp {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl NtpTimestampGenerator for EmbassyTimestamp {
    fn init(&mut self) {
        self.start = Instant::now();
    }

    fn timestamp_sec(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        (self.start.elapsed().as_micros() % 1_000_000) as u32
    }
}

/// Perform a single NTP sync, returning Unix timestamp if successful
async fn sync(stack: embassy_net::Stack<'static>) -> Option<u64> {
    info!(
        "[ntp] starting sync with {}.{}.{}.{}",
        NTP_SERVER_IP[0], NTP_SERVER_IP[1], NTP_SERVER_IP[2], NTP_SERVER_IP[3]
    );

    // Create UDP socket
    info!("[ntp] creating UDP socket");
    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buffer = [0u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buffer = [0u8; 128];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );

    // Bind to any local port
    info!("[ntp] binding socket");
    if let Err(e) = socket.bind(0) {
        warn!("[ntp] bind failed: {:?}", e);
        return None;
    }
    info!("[ntp] socket bound");

    // Create socket address for sntpc using hardcoded Cloudflare NTP IP
    let server_addr = core::net::SocketAddr::new(
        core::net::IpAddr::V4(core::net::Ipv4Addr::new(
            NTP_SERVER_IP[0],
            NTP_SERVER_IP[1],
            NTP_SERVER_IP[2],
            NTP_SERVER_IP[3],
        )),
        NTP_PORT,
    );

    // Create NTP context and timestamp generator
    let ntp_context = NtpContext::new(EmbassyTimestamp::new());

    // Get time from NTP server with timeout
    info!("[ntp] sending request (timeout {}s)", NTP_TIMEOUT_SECS);
    let ntp_future = sntpc::get_time(server_addr, &socket, ntp_context);
    match with_timeout(Duration::from_secs(NTP_TIMEOUT_SECS), ntp_future).await {
        Ok(Ok(time)) => {
            let unix_secs = time.sec() as u64;
            info!("[ntp] sync successful: {} seconds", unix_secs);
            Some(unix_secs)
        }
        Ok(Err(e)) => {
            warn!("[ntp] sync failed: {:?}", e);
            None
        }
        Err(_) => {
            warn!("[ntp] sync timeout after {}s", NTP_TIMEOUT_SECS);
            None
        }
    }
}

/// NTP sync task - syncs time every 12 hours, retrying every 30 seconds after a failure
#[embassy_executor::task]
async fn ntp_task(stack: embassy_net::Stack<'static>) -> ! {
    loop {
        let delay_secs = match sync(stack).await {
            Some(unix_secs) => {
                set_unix_time(unix_secs);
                SYNC_INTERVAL_SECS
            }
            None => {
                warn!("[ntp] retrying in {}s", RETRY_INTERVAL_SECS);
                RETRY_INTERVAL_SECS
            }
        };
        Timer::after_secs(delay_secs).await;
    }
}

/// Initialize NTP: spawn background task for time synchronization
///
/// The background task will sync immediately and then every 12 hours,
/// retrying every 30 seconds until a sync succeeds.
/// This function returns immediately without blocking.
pub fn init(spawner: &Spawner, stack: embassy_net::Stack<'static>) {
    info!("[ntp] spawning background sync task");
    spawner.spawn(ntp_task(stack)).ok();
}
