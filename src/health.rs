//! Health: watchdog, reset cause, liveness and status shared across tasks
//!
//! ## Watchdog
//! The RP2040 hardware watchdog reboots the chip if it isn't fed within
//! WATCHDOG_TIMEOUT. `watchdog_task` (Core 0) feeds it once a second, but only
//! while every sensor task on Core 1 is still looping (each SENSOR_LOOPS[i]
//! changed within STALL_LIMIT).
//! So it reboots if either:
//! - Core 0's executor hangs (the feeding task never runs), or
//! - Core 1 hangs or crashes, or one sensor task gets stuck (a SENSOR_LOOPS
//!   counter stops changing).
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
use portable_atomic::{AtomicU32, AtomicU8, Ordering};

/// Reboot if the watchdog isn't fed for this long (RP2040 max is ~8.3 s)
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// How often watchdog_task checks liveness and feeds the watchdog
const FEED_INTERVAL: Duration = Duration::from_secs(1);

/// Marker stored in watchdog scratch register 0 by the panic handler.
/// (Scratch 4-7 are reserved by the bootrom; 0-3 are free.)
const PANIC_MAGIC: u32 = 0x5041_4E43; // "PANC"

/// Stop feeding if any sensor loop counter hasn't changed for this long
const STALL_LIMIT: Duration = Duration::from_secs(3);

/// Iterations of each sensor task (Core 1), counted whether or not the read
/// succeeded; watched by watchdog_task (Core 0)
pub static SENSOR_LOOPS: [AtomicU32; NUM_SENSORS] = [const { AtomicU32::new(0) }; NUM_SENSORS];

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
// WiFi state (for status display)
// ---------------------------------------------------------------------------

/// WiFi connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
#[repr(u8)]
pub enum WifiState {
    /// Trying to join, no failure yet
    Joining = 0,
    /// Joined and network stack up
    Connected = 1,
    /// Join attempts are failing (still retrying)
    Failing = 2,
}

static WIFI_STATE: AtomicU8 = AtomicU8::new(WifiState::Joining as u8);

/// Record the WiFi state
pub fn set_wifi_state(state: WifiState) {
    WIFI_STATE.store(state as u8, Ordering::Relaxed);
}

/// Current WiFi state
pub fn wifi_state() -> WifiState {
    match WIFI_STATE.load(Ordering::Relaxed) {
        1 => WifiState::Connected,
        2 => WifiState::Failing,
        _ => WifiState::Joining,
    }
}

// ---------------------------------------------------------------------------
// Watchdog task
// ---------------------------------------------------------------------------

/// Starts the watchdog and feeds it while every sensor task keeps looping.
#[embassy_executor::task]
pub async fn watchdog_task(mut watchdog: Watchdog) -> ! {
    info!("[watchdog] starting, timeout {}ms", WATCHDOG_TIMEOUT.as_millis());
    watchdog.start(WATCHDOG_TIMEOUT);

    let mut last_loops = [0u32; NUM_SENSORS];
    let mut last_change = [Instant::now(); NUM_SENSORS];
    loop {
        Timer::after(FEED_INTERVAL).await;

        let mut all_alive = true;
        for i in 0..NUM_SENSORS {
            let loops = SENSOR_LOOPS[i].load(Ordering::Relaxed);
            if loops != last_loops[i] {
                last_loops[i] = loops;
                last_change[i] = Instant::now();
            } else if last_change[i].elapsed() > STALL_LIMIT {
                warn!("[watchdog] sensor {} loop stalled at {}; not feeding", i, loops);
                all_alive = false;
            }
        }
        if all_alive {
            watchdog.feed();
        }
    }
}

// ---------------------------------------------------------------------------
// Panic handler support
// ---------------------------------------------------------------------------

/// Log a panic, mark it in a scratch register, and reboot via the watchdog.
/// Call from the binary's `#[panic_handler]`.
pub fn panic_reboot(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("[panic] {}", defmt::Display2Format(info));

    let watchdog = pac::WATCHDOG;
    watchdog.scratch0().write(|w| *w = PANIC_MAGIC);

    // ~200 ms at 125 MHz so an attached debug probe can read the log before reset
    cortex_m::asm::delay(25_000_000);

    // Reset everything except the oscillators (same as embassy's Watchdog::trigger_reset)
    pac::PSM
        .wdsel()
        .write_value(pac::psm::regs::Wdsel(0x0001_ffff & !(1 << 0) & !(1 << 1)));
    watchdog.ctrl().write(|w| w.set_trigger(true));

    loop {
        cortex_m::asm::nop();
    }
}
