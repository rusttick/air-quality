#![no_std]

pub mod health;
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
use portable_atomic::{AtomicI16, Ordering};

use sen55::{DeviceStatus, Measurement, PmNumbers};

// Shared interrupt bindings - used by both Core 0 (WiFi/ADC) and Core 1 (I2C sensor)
// All interrupt handlers must be in a single struct for proper routing
bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;  // WiFi (Core 0)
    ADC_IRQ_FIFO => AdcInterruptHandler;      // ADC (Core 0)
    I2C0_IRQ => I2cInterruptHandler<I2C0>;    // SEN55 #0 inlet (Core 1)
    I2C1_IRQ => I2cInterruptHandler<I2C1>;    // SEN55 #1 outlet (Core 1)
});

/// System uptime in seconds. The RP2040 timer is reset by `embassy_rp::init`,
/// so embassy's clock counts from boot.
pub fn uptime_secs() -> u64 {
    Instant::now().as_secs()
}

/// `TEMP_DECI_C` before the first reading
const TEMP_UNKNOWN: i16 = i16::MIN;

/// MCU temperature in 0.1°C units (e.g. 325 = 32.5°C), updated by temp_task
pub static TEMP_DECI_C: AtomicI16 = AtomicI16::new(TEMP_UNKNOWN);

/// MCU temperature [°C]; None until temp_task has published its first reading
pub fn mcu_temp_c() -> Option<f64> {
    let deci = TEMP_DECI_C.load(Ordering::Relaxed);
    (deci != TEMP_UNKNOWN).then(|| deci as f64 / 10.0)
}

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
