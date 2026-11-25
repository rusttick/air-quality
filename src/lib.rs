#![no_std]

pub mod health;
pub mod ntp;
pub mod sen55;
pub mod sensor_task;
pub mod temp_task;

use embassy_rp::adc::InterruptHandler as AdcInterruptHandler;
use embassy_rp::bind_interrupts;
use embassy_rp::i2c::InterruptHandler as I2cInterruptHandler;
use embassy_rp::peripherals::{I2C0, I2C1, PIO0};
use embassy_rp::pio::InterruptHandler as PioInterruptHandler;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::Instant;
use portable_atomic::{AtomicI16, AtomicU64, Ordering};

use sen55::{DeviceStatus, Measurement, PmNumbers};

// Shared interrupt bindings - used by both Core 0 (WiFi/ADC) and Core 1 (I2C sensor)
// All interrupt handlers must be in a single struct for proper routing
bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;  // WiFi (Core 0)
    ADC_IRQ_FIFO => AdcInterruptHandler;      // ADC (Core 0)
    I2C0_IRQ => I2cInterruptHandler<I2C0>;    // SEN55 #0 inlet (Core 1)
    I2C1_IRQ => I2cInterruptHandler<I2C1>;    // SEN55 #1 outlet (Core 1)
});

// Boot time tracking for uptime calculation
static BOOT_TICKS: AtomicU64 = AtomicU64::new(0);

/// Initialize boot time - call at the very start of main()
pub fn init_boot_time() {
    BOOT_TICKS.store(Instant::now().as_ticks(), Ordering::Relaxed);
}

/// Get system uptime in seconds since boot
pub fn uptime_secs() -> u64 {
    let boot_ticks = BOOT_TICKS.load(Ordering::Relaxed);
    let boot = Instant::from_ticks(boot_ticks);
    boot.elapsed().as_secs()
}

// Unix time tracking - stores unix timestamp at boot (computed from NTP sync)
static UNIX_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// Called by NTP task when time is synced
pub fn set_unix_time(unix_secs: u64) {
    let unix_at_boot = unix_secs.saturating_sub(uptime_secs());
    UNIX_AT_BOOT.store(unix_at_boot, Ordering::Relaxed);
}

/// Get current Unix time (0 if NTP hasn't synced yet)
pub fn unix_time() -> u64 {
    let unix_at_boot = UNIX_AT_BOOT.load(Ordering::Relaxed);
    if unix_at_boot == 0 {
        return 0;
    }
    unix_at_boot + uptime_secs()
}

/// MCU temperature in 0.1°C units (e.g. 325 = 32.5°C), updated by temp_task
pub static TEMP_DECI_C: AtomicI16 = AtomicI16::new(0);

// ============================================================================
// SEN55 data shared from Core 1 (sensor tasks) to Core 0 (web)
// ============================================================================

/// Number of SEN55 sensors, one per hardware I2C bus
pub const NUM_SENSORS: usize = 2;

/// Sensor names by index: 0 = I2C0 (GP4/GP5), 1 = I2C1 (GP6/GP7)
pub const SENSOR_NAMES: [&str; NUM_SENSORS] = ["inlet", "outlet"];

/// SEN55 identity, read once each time the sensor is (re)initialized
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SensorInfo {
    pub product: heapless::String<32>,
    pub serial: heapless::String<32>,
    pub firmware: u8,
}

/// One SEN55 reading, published once per second while measuring
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct SensorData {
    /// Increments with every published reading
    pub seq: u32,
    /// Uptime when this reading was taken [s]
    pub uptime_secs: u64,
    /// Seconds since Start Measurement; use to judge warm-up
    /// (PM ~30 s, VOC ~1 h, NOx ~6 h to meet spec)
    pub measuring_secs: u64,
    pub measurement: Measurement,
    /// Number concentrations (0x0413); None if the read failed / unsupported firmware
    pub pm_numbers: Option<PmNumbers>,
    /// Last device status read (refreshed every few seconds)
    pub status: DeviceStatus,
}

/// Latest reading per sensor (web reads via try_get; 1 receiver slot spare for an async consumer)
pub static SENSOR_DATA: [Watch<CriticalSectionRawMutex, SensorData, 1>; NUM_SENSORS] =
    [const { Watch::new() }; NUM_SENSORS];

/// Identity per sensor (None until first successful init)
pub static SENSOR_INFO: [Watch<CriticalSectionRawMutex, SensorInfo, 1>; NUM_SENSORS] =
    [const { Watch::new() }; NUM_SENSORS];
