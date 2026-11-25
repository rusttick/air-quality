//! Sensor Tasks (Core 1)
//!
//! One task per SEN55, each owning its own hardware I2C bus (the SEN55 address
//! 0x69 is fixed, so two sensors can't share a bus):
//!
//! | idx | name   | bus  | SDA        | SCL         |
//! |-----|--------|------|------------|-------------|
//! | 0   | inlet  | I2C0 | GP4 (pin 6)| GP5 (pin 7) |
//! | 1   | outlet | I2C1 | GP6 (pin 9)| GP7 (pin 10)|
//!
//! Each task, once a second:
//! - bumps `health::SENSOR_LOOPS[idx]` (the watchdog on Core 0 checks it is still moving)
//! - if the sensor isn't running: reset, read identity, start measurement
//! - otherwise: if data is ready, read values + number concentrations and
//!   publish them to `SENSOR_DATA[idx]` for the web API on Core 0
//!
//! After `MAX_CONSECUTIVE_ERRORS` failed cycles the sensor is re-initialized,
//! so unplugging / replugging a sensor recovers without a reboot.
//!
//! Both sensors: VDD = 5 V (VBUS), SEL = GND, ~10 kΩ pull-ups to 3V3 on SDA/SCL.

use defmt::{info, warn, Debug2Format};
use embassy_rp::i2c::{self, I2c};
use embassy_rp::peripherals::{I2C0, I2C1, PIN_4, PIN_5, PIN_6, PIN_7};
use embassy_rp::Peri;
use embassy_time::{with_timeout, Duration, Instant, Ticker};
use embedded_hal_async::i2c::I2c as I2cBus;
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

use crate::health::SENSOR_LOOPS;
use crate::sen55::{DeviceStatus, Error, Measurement, PmNumbers, Sen55};
use crate::{
    uptime_secs, Irqs, SensorData, SensorInfo, NUM_SENSORS, SENSOR_DATA, SENSOR_INFO,
    SENSOR_NAMES,
};

/// SEN55 maximum I2C speed (standard mode)
const I2C_FREQUENCY: u32 = 100_000;

/// Loop period; the SEN55 produces a new reading every second
const PERIOD: Duration = Duration::from_secs(1);

/// Upper bound for one init sequence (reset 100 ms + reads + start 50 ms)
const INIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound for one read cycle (normally ~70 ms)
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Read the device status register every N cycles
const STATUS_EVERY: u32 = 5;

/// Re-initialize the sensor after this many failed read cycles in a row
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// While init keeps failing (e.g. sensor unplugged), log only every N attempts
const INIT_LOG_EVERY: u32 = 60;

/// True while sensor[idx] is initialized and measuring
pub static SENSOR_ONLINE: [AtomicBool; NUM_SENSORS] = [const { AtomicBool::new(false) }; NUM_SENSORS];

/// Total failed init/read cycles since boot, per sensor
pub static SENSOR_ERRORS: [AtomicU32; NUM_SENSORS] = [const { AtomicU32::new(0) }; NUM_SENSORS];

/// Peripherals for sensor 0 (inlet): I2C0 on GP4/GP5
pub struct Sensor0Resources {
    pub i2c: Peri<'static, I2C0>,
    pub sda: Peri<'static, PIN_4>,
    pub scl: Peri<'static, PIN_5>,
}

/// Peripherals for sensor 1 (outlet): I2C1 on GP6/GP7
pub struct Sensor1Resources {
    pub i2c: Peri<'static, I2C1>,
    pub sda: Peri<'static, PIN_6>,
    pub scl: Peri<'static, PIN_7>,
}

fn i2c_config() -> i2c::Config {
    let mut config = i2c::Config::default();
    config.frequency = I2C_FREQUENCY;
    config
}

#[embassy_executor::task]
pub async fn sensor0_task(r: Sensor0Resources) -> ! {
    let bus = I2c::new_async(r.i2c, r.scl, r.sda, Irqs, i2c_config());
    run(0, Sen55::new(bus)).await
}

#[embassy_executor::task]
pub async fn sensor1_task(r: Sensor1Resources) -> ! {
    let bus = I2c::new_async(r.i2c, r.scl, r.sda, Irqs, i2c_config());
    run(1, Sen55::new(bus)).await
}

/// Reset the sensor, read its identity, and start measurement
async fn init<B: I2cBus>(sen: &mut Sen55<B>) -> Result<SensorInfo, Error<B::Error>> {
    sen.reset().await?;
    let info = SensorInfo {
        product: sen.product_name().await?,
        serial: sen.serial_number().await?,
        firmware: sen.firmware_version().await?,
    };
    // Sticky fan/laser errors survive a soft reset; start clean
    sen.clear_status().await?;
    sen.start_measurement().await?;
    Ok(info)
}

type Reading = (Measurement, Option<PmNumbers>, Option<DeviceStatus>);

/// One read cycle. Returns None if no new data was ready.
async fn read<B: I2cBus>(
    sen: &mut Sen55<B>,
    read_status: bool,
) -> Result<Option<Reading>, Error<B::Error>> {
    let status = if read_status { Some(sen.read_status().await?) } else { None };

    if !sen.data_ready().await? {
        return Ok(None);
    }
    let measurement = sen.read_measurement().await?;

    // 0x0413 needs firmware > 0.7; treat failure as "not available" rather than an error
    let pm_numbers = sen.read_pm_numbers().await.ok();

    Ok(Some((measurement, pm_numbers, status)))
}

/// Sensor loop shared by both tasks; `idx` selects the shared-state slot
async fn run<B: I2cBus>(idx: usize, mut sen: Sen55<B>) -> ! {
    let name = SENSOR_NAMES[idx];
    info!("[sensor {}] {} task starting on core 1", idx, name);

    let sender = SENSOR_DATA[idx].sender();
    let mut ticker = Ticker::every(PERIOD);

    let mut running = false;
    let mut consecutive_errors: u32 = 0;
    let mut started_at = Instant::now();
    let mut cycle: u32 = 0;
    let mut seq: u32 = 0;
    let mut status = DeviceStatus::default();

    loop {
        ticker.next().await;
        SENSOR_LOOPS[idx].fetch_add(1, Ordering::Relaxed);
        cycle = cycle.wrapping_add(1);

        if !running {
            let result = with_timeout(INIT_TIMEOUT, init(&mut sen)).await;
            match result {
                Ok(Ok(info)) => {
                    info!(
                        "[sensor {}] {}: {} serial {} firmware {}: measuring",
                        idx,
                        name,
                        info.product.as_str(),
                        info.serial.as_str(),
                        info.firmware
                    );
                    SENSOR_INFO[idx].sender().send(info);
                    running = true;
                    consecutive_errors = 0;
                    started_at = Instant::now();
                    SENSOR_ONLINE[idx].store(true, Ordering::Relaxed);
                }
                Ok(Err(e)) => {
                    SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                    if consecutive_errors % INIT_LOG_EVERY == 0 {
                        warn!("[sensor {}] {} init failed: {}", idx, name, Debug2Format(&e));
                    }
                    consecutive_errors += 1;
                }
                Err(_) => {
                    SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                    if consecutive_errors % INIT_LOG_EVERY == 0 {
                        warn!("[sensor {}] {} init timed out", idx, name);
                    }
                    consecutive_errors += 1;
                }
            }
            continue;
        }

        let read_status = cycle % STATUS_EVERY == 0;
        match with_timeout(READ_TIMEOUT, read(&mut sen, read_status)).await {
            Ok(Ok(result)) => {
                consecutive_errors = 0;
                let Some((measurement, pm_numbers, new_status)) = result else {
                    continue;
                };
                if let Some(s) = new_status {
                    if s.has_error() && s != status {
                        warn!("[sensor {}] {} device status error: {}", idx, name, s);
                    }
                    status = s;
                }
                seq = seq.wrapping_add(1);
                sender.send(SensorData {
                    seq,
                    uptime_secs: uptime_secs(),
                    measuring_secs: started_at.elapsed().as_secs(),
                    measurement,
                    pm_numbers,
                    status,
                });
                if seq % 60 == 0 {
                    info!("[sensor {}] {}: {}", idx, name, measurement);
                }
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                warn!(
                    "[sensor {}] {} read failed ({} in a row): {}",
                    idx,
                    name,
                    consecutive_errors,
                    Debug2Format(&e)
                );
            }
            Err(_) => {
                consecutive_errors += 1;
                SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                warn!("[sensor {}] {} read timed out ({} in a row)", idx, name, consecutive_errors);
            }
        }

        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            warn!("[sensor {}] {} too many errors; re-initializing", idx, name);
            running = false;
            consecutive_errors = 0;
            SENSOR_ONLINE[idx].store(false, Ordering::Relaxed);
        }
    }
}
