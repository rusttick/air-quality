//! Health: watchdog, reset cause, liveness and status shared across tasks
//!
//! ## Watchdog
//! The RP2040 hardware watchdog reboots the chip if it isn't fed within
//! WATCHDOG_TIMEOUT. `watchdog_task` (Core 0) feeds it once a second, but only
//! while every sensor task on Core 1 is still looping (each SENSOR_LOOPS[i]
//! changed within STALL_LIMIT) and the network loop on Core 0 is still
//! advancing (NET_LOOPS changed within NET_STALL_LIMIT).
//! So it reboots if any of these happen:
//! - Core 0's executor hangs (the feeding task never runs)
//! - Core 1 hangs or crashes, or one sensor task gets stuck (a SENSOR_LOOPS
//!   counter stops changing)
//! - the WiFi chip stops answering, so the network loop is stuck waiting for
//!   it (NET_LOOPS stops changing). The reboot power-cycles the WiFi chip.
//!
//! An access point that is down does not cause a reboot: join attempts fail or
//! time out, and each attempt counts as progress.
//!
//! While a debugger has a core halted, the watchdog is paused (hardware default).
//!
//! ## Panics
//! `panic_reboot` logs the panic, leaves a marker in a watchdog scratch
//! register, and reboots via the watchdog. On the next boot `read_reset_cause`
//! reports it as `ResetCause::Panic`.

use defmt::{info, warn};
use embassy_rp::pac;
use embassy_rp::watchdog::{ResetReason, Watchdog};
use embassy_time::{Duration, Instant, Timer};

use crate::NUM_SENSORS;
use portable_atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

/// Reboot if the watchdog isn't fed for this long (RP2040 max is ~8.3 s)
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// How often watchdog_task checks liveness and feeds the watchdog
const FEED_INTERVAL: Duration = Duration::from_secs(1);

/// Marker stored in watchdog scratch register 0 by the panic handler.
/// (Scratch 4-7 are reserved by the bootrom; 0-3 are free.)
const PANIC_MAGIC: u32 = 0x5041_4E43; // "PANC"

/// Stop feeding if any sensor loop counter hasn't changed for this long.
/// The longest gap in a healthy sensor task is one timed-out init (2 s).
const STALL_LIMIT: Duration = Duration::from_secs(3);

/// Stop feeding if the network loop counter hasn't changed for this long.
/// Must be well above the longest healthy step, one WiFi join attempt
/// (`JOIN_TIMEOUT` in main.rs, 30 s), and above the WiFi chip's start-up
/// (firmware upload, a few seconds).
const NET_STALL_LIMIT: Duration = Duration::from_secs(90);

/// Progress of each sensor task (Core 1), counted whether or not the read
/// succeeded; watched by watchdog_task (Core 0)
pub static SENSOR_LOOPS: [AtomicU32; NUM_SENSORS] = [const { AtomicU32::new(0) }; NUM_SENSORS];

/// Progress of the network loop (Core 0): every WiFi join attempt and every
/// link check; watched by watchdog_task
pub static NET_LOOPS: AtomicU32 = AtomicU32::new(0);

/// Times WiFi was rejoined because the link looked dead (for /api/status)
pub static WIFI_REJOINS: AtomicU32 = AtomicU32::new(0);

/// Call from the network loop whenever it makes progress
pub fn net_alive() {
    NET_LOOPS.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Reset cause
// ---------------------------------------------------------------------------

/// Why the chip last reset
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
#[repr(u8)]
pub enum ResetCause {
    /// Power-on, RUN pin, or debugger reset (not the watchdog)
    PowerOn = 0,
    /// Watchdog timeout: something hung
    WatchdogTimeout = 1,
    /// Panic handler reboot
    Panic = 2,
    /// Watchdog reset forced by software (not a panic)
    Forced = 3,
}

impl ResetCause {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::WatchdogTimeout,
            2 => Self::Panic,
            3 => Self::Forced,
            _ => Self::PowerOn,
        }
    }

    /// Short name for logs and the JSON API
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PowerOn => "power_on",
            Self::WatchdogTimeout => "watchdog",
            Self::Panic => "panic",
            Self::Forced => "forced",
        }
    }

    /// True if the last reset means something went wrong
    pub fn is_fault(self) -> bool {
        matches!(self, Self::WatchdogTimeout | Self::Panic)
    }
}

static RESET_CAUSE: AtomicU8 = AtomicU8::new(ResetCause::PowerOn as u8);

/// Read and record why the chip last reset. Call once at boot, before starting the watchdog.
pub fn read_reset_cause(watchdog: &mut Watchdog) -> ResetCause {
    let marker = watchdog.get_scratch(0);
    watchdog.set_scratch(0, 0);

    let cause = if marker == PANIC_MAGIC {
        ResetCause::Panic
    } else {
        match watchdog.reset_reason() {
            Some(ResetReason::TimedOut) => ResetCause::WatchdogTimeout,
            Some(ResetReason::Forced) => ResetCause::Forced,
            None => ResetCause::PowerOn,
        }
    };
    RESET_CAUSE.store(cause as u8, Ordering::Relaxed);
    cause
}

/// Why the chip last reset (as recorded at boot)
pub fn reset_cause() -> ResetCause {
    ResetCause::from_u8(RESET_CAUSE.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Watchdog task
// ---------------------------------------------------------------------------

/// Tracks whether one progress counter is still changing
struct Progress {
    last: u32,
    changed_at: Instant,
}

impl Progress {
    fn new() -> Self {
        Self {
            last: 0,
            changed_at: Instant::now(),
        }
    }

    /// True if `count` has changed within `limit`
    fn is_alive(&mut self, count: u32, limit: Duration) -> bool {
        if count != self.last {
            self.last = count;
            self.changed_at = Instant::now();
            true
        } else {
            self.changed_at.elapsed() <= limit
        }
    }
}

/// Starts the watchdog and feeds it while every sensor task and the network
/// loop keep making progress.
#[embassy_executor::task]
pub async fn watchdog_task(mut watchdog: Watchdog) -> ! {
    info!("[watchdog] starting, timeout {}ms", WATCHDOG_TIMEOUT.as_millis());
    watchdog.start(WATCHDOG_TIMEOUT);

    let mut sensors: [Progress; NUM_SENSORS] = core::array::from_fn(|_| Progress::new());
    let mut net = Progress::new();
    loop {
        Timer::after(FEED_INTERVAL).await;

        let mut all_alive = true;
        for (i, sensor) in sensors.iter_mut().enumerate() {
            if !sensor.is_alive(SENSOR_LOOPS[i].load(Ordering::Relaxed), STALL_LIMIT) {
                warn!("[watchdog] sensor {} loop stalled; not feeding", i);
                all_alive = false;
            }
        }
        if !net.is_alive(NET_LOOPS.load(Ordering::Relaxed), NET_STALL_LIMIT) {
            warn!("[watchdog] network loop stalled; not feeding");
            all_alive = false;
        }
        if all_alive {
            watchdog.feed();
        }
    }
}

// ---------------------------------------------------------------------------
// Panic handler support
// ---------------------------------------------------------------------------

/// Set once a panic is being handled, on either core
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Log a panic, mark it in a scratch register, and reboot via the watchdog.
/// Call from the binary's `#[panic_handler]`.
pub fn panic_reboot(info: &core::panic::PanicInfo) -> ! {
    // Stop this core's interrupt handlers from running on top of the panic
    cortex_m::interrupt::disable();

    // Mark first: if anything below goes wrong, the hardware watchdog still
    // resets the chip and the next boot still reports a panic
    let watchdog = pac::WATCHDOG;
    watchdog.scratch0().write(|w| *w = PANIC_MAGIC);

    // Log only the first panic. Logging can itself panic (e.g. a panic inside
    // a log statement, while the logger is held), which would recurse.
    if !PANICKING.swap(true, Ordering::Relaxed) {
        defmt::error!("[panic] {}", defmt::Display2Format(info));

        // About 0.3 s at 125 MHz so an attached debug probe can read the log before reset
        cortex_m::asm::delay(25_000_000);
    }

    // Reset everything except the oscillators (same as embassy's Watchdog::trigger_reset)
    pac::PSM
        .wdsel()
        .write_value(pac::psm::regs::Wdsel(0x0001_ffff & !(1 << 0) & !(1 << 1)));
    watchdog.ctrl().write(|w| w.set_trigger(true));

    loop {
        cortex_m::asm::nop();
    }
}
