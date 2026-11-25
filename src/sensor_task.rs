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
//! Recovery (no reboot needed to unplug / replug a sensor):
//! - `MAX_CONSECUTIVE_ERRORS` failed read cycles in a row: re-initialize the sensor
//! - `MAX_IDLE_CYCLES` cycles without new data although I2C works: re-initialize.
//!   This is a sensor that lost power briefly and came back in Idle mode.
//! - before every (re)initialization and after any timed-out transaction: free
//!   the bus (`bus_clear`) and reset the RP2040's I2C block. The sensors stay
//!   powered while the RP2040 resets, so a reset in the middle of a read can
//!   leave a sensor holding SDA low.
//!
//! Both sensors: VDD = 5 V (VBUS), SEL = GND, external pull-ups to 3V3 on SDA/SCL
//! (3.3 kΩ for ~3 m cables, see doc/wiring_cat6.md and doc/wiring_svideo.md).

use defmt::{info, warn, Debug2Format};
use embassy_rp::gpio::{self, Level, OutputOpenDrain};
use embassy_rp::i2c::{self, I2c, SclPin, SdaPin};
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::peripherals::{I2C0, I2C1, PIN_4, PIN_5, PIN_6, PIN_7};
use embassy_rp::Peri;
use embassy_time::{with_timeout, Duration, Instant, Ticker, Timer};
use embedded_hal_async::i2c::I2c as I2cBus;
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

use crate::health::SENSOR_LOOPS;
use crate::sen55::{DeviceStatus, Error, Measurement, PmNumbers, Sen55};
use crate::{
    uptime_secs, Irqs, SensorData, SensorInfo, NUM_SENSORS, SENSOR_DATA, SENSOR_INFO,
    SENSOR_NAMES,
};

/// I2C clock. The SEN55 allows up to 100 kHz; 10 kHz gives a large margin on
/// long cables (~3 m) and one read cycle still takes only ~150 ms.
///
/// Check the glitch filter before changing this: embassy-rp sets its length to
/// `(low_count / 16) as u8` clk_peri cycles, which wraps. 10 kHz gives 212
/// cycles (1.7 µs at 125 MHz), but the filter is nearly off just below
/// 18.3 kHz and again just below 9.15 kHz.
const I2C_FREQUENCY: u32 = 10_000;

/// Loop period; the SEN55 produces a new reading every second
const PERIOD: Duration = Duration::from_secs(1);

/// Upper bound for one init sequence (~350 ms at 10 kHz: reset 100 ms + reads + start 50 ms).
/// Keep PERIOD + INIT_TIMEOUT within `health::STALL_LIMIT`.
const INIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound for one read cycle (~150 ms at 10 kHz)
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Read the device status register every N cycles
const STATUS_EVERY: u32 = 5;

/// Re-initialize the sensor after this many failed read cycles in a row
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Re-initialize the sensor after this many cycles in a row where it answered
/// but had no new data. Must stay well above the 10 s fan cleaning, during
/// which the sensor doesn't update its values.
const MAX_IDLE_CYCLES: u32 = 30;

/// While init keeps failing (e.g. sensor unplugged), log only every N attempts
const INIT_LOG_EVERY: u32 = 60;

/// Half an SCL period while bit-banging `bus_clear` (5 kHz)
const BUS_CLEAR_HALF_PERIOD: Duration = Duration::from_micros(100);

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
    // embassy-rp enables the RP2040's internal pull-ups by default; the board
    // has external pull-ups sized for the cable, so turn the internal ones off
    config.sda_pullup = false;
    config.scl_pullup = false;
    config
}

#[embassy_executor::task]
pub async fn sensor0_task(r: Sensor0Resources) -> ! {
    run(0, r.i2c, r.scl, r.sda).await
}

#[embassy_executor::task]
pub async fn sensor1_task(r: Sensor1Resources) -> ! {
    run(1, r.i2c, r.scl, r.sda).await
}

/// Free an I2C bus that a sensor is holding (I2C specification, "bus clear").
///
/// A sensor that was interrupted in the middle of sending a byte keeps driving
/// SDA until it has been clocked through the rest of that byte. The RP2040's
/// I2C block can't start a transaction while SDA is low, so clock SCL by hand
/// until the sensor lets go, then send a STOP.
///
/// Returns false if SCL or SDA is still low afterwards (short circuit, or a
/// missing pull-up).
async fn bus_clear(scl: Peri<'_, impl gpio::Pin>, sda: Peri<'_, impl gpio::Pin>) -> bool {
    // Open drain: drive low or release, and let the external pull-ups pull high
    let mut scl = OutputOpenDrain::new(scl, Level::High);
    let mut sda = OutputOpenDrain::new(sda, Level::High);
    Timer::after(BUS_CLEAR_HALF_PERIOD).await;
    if scl.is_high() && sda.is_high() {
        return true;
    }

    // At most 9 clocks: 8 data bits, then the sensor sees a NACK and stops sending
    for _ in 0..9 {
        if sda.is_high() {
            break;
        }
        scl.set_low();
        Timer::after(BUS_CLEAR_HALF_PERIOD).await;
        scl.set_high();
        Timer::after(BUS_CLEAR_HALF_PERIOD).await;
    }

    // STOP: SDA rises while SCL is high
    scl.set_low();
    Timer::after(BUS_CLEAR_HALF_PERIOD).await;
    sda.set_low();
    Timer::after(BUS_CLEAR_HALF_PERIOD).await;
    scl.set_high();
    Timer::after(BUS_CLEAR_HALF_PERIOD).await;
    sda.set_high();
    Timer::after(BUS_CLEAR_HALF_PERIOD).await;

    scl.is_high() && sda.is_high()
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

type Reading = (Measurement, Option<PmNumbers>);

/// One read cycle: the device status (if requested), and the new reading if
/// one was ready
async fn read<B: I2cBus>(
    sen: &mut Sen55<B>,
    read_status: bool,
) -> Result<(Option<DeviceStatus>, Option<Reading>), Error<B::Error>> {
    let status = if read_status { Some(sen.read_status().await?) } else { None };

    if !sen.data_ready().await? {
        return Ok((status, None));
    }
    let measurement = sen.read_measurement().await?;

    // 0x0413 needs firmware > 0.7; treat failure as "not available" rather than an error
    let pm_numbers = sen.read_pm_numbers().await.ok();

    Ok((status, Some((measurement, pm_numbers))))
}

/// Sensor loop shared by both tasks; `idx` selects the shared-state slot
async fn run<T, SCL, SDA>(
    idx: usize,
    mut i2c: Peri<'static, T>,
    mut scl: Peri<'static, SCL>,
    mut sda: Peri<'static, SDA>,
) -> !
where
    T: i2c::Instance,
    SCL: SclPin<T>,
    SDA: SdaPin<T>,
    Irqs: Binding<T::Interrupt, i2c::InterruptHandler<T>>,
{
    let name = SENSOR_NAMES[idx];
    info!("[sensor {}] {} task starting on core 1", idx, name);

    let alive = || {
        SENSOR_LOOPS[idx].fetch_add(1, Ordering::Relaxed);
    };
    let sender = SENSOR_DATA[idx].sender();
    let mut ticker = Ticker::every(PERIOD);

    let mut running = false;
    // Consecutive failed init attempts (for log throttling)
    let mut init_failures: u32 = 0;
    // Consecutive failed read cycles
    let mut consecutive_errors: u32 = 0;
    // Consecutive read cycles where the sensor answered but had no new data
    let mut idle_cycles: u32 = 0;
    let mut started_at = Instant::now();
    let mut cycle: u32 = 0;
    let mut seq: u32 = 0;
    let mut status = DeviceStatus::default();

    // The first tick also gives the sensor its power-up time (50 ms) after a cold start
    ticker.next().await;

    loop {
        // A fresh bus: release a stuck sensor, then (re)create the driver, which
        // resets the RP2040's I2C block. The inner loop runs on it until a
        // transaction times out or the sensor needs re-initializing.
        let bus_free = bus_clear(scl.reborrow(), sda.reborrow()).await;
        if !bus_free && init_failures % INIT_LOG_EVERY == 0 {
            warn!("[sensor {}] {} I2C bus is stuck low (check wiring and pull-ups)", idx, name);
        }
        let bus = I2c::new_async(i2c.reborrow(), scl.reborrow(), sda.reborrow(), Irqs, i2c_config());
        let mut sen = Sen55::new(bus);

        loop {
            alive();
            cycle = cycle.wrapping_add(1);
            let mut rebuild_bus = false;

            if !running {
                let result = with_timeout(INIT_TIMEOUT, init(&mut sen)).await;
                // Init can outlast PERIOD: show the watchdog we're alive, and restart
                // the ticker so the missed ticks aren't replayed as a burst
                alive();
                ticker.reset();
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
                        init_failures = 0;
                        consecutive_errors = 0;
                        idle_cycles = 0;
                        status = DeviceStatus::default();
                        started_at = Instant::now();
                        SENSOR_ONLINE[idx].store(true, Ordering::Relaxed);
                    }
                    failed => {
                        SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                        if init_failures % INIT_LOG_EVERY == 0 {
                            match failed {
                                Ok(Err(e)) => {
                                    warn!("[sensor {}] {} init failed: {}", idx, name, Debug2Format(&e))
                                }
                                _ => warn!("[sensor {}] {} init timed out", idx, name),
                            }
                        }
                        init_failures = init_failures.wrapping_add(1);
                        rebuild_bus = true;
                    }
                }
            } else {
                let read_status = cycle % STATUS_EVERY == 0;
                match with_timeout(READ_TIMEOUT, read(&mut sen, read_status)).await {
                    Ok(Ok((new_status, reading))) => {
                        consecutive_errors = 0;
                        if let Some(s) = new_status {
                            if s.has_error() && s != status {
                                warn!("[sensor {}] {} device status error: {}", idx, name, s);
                            }
                            status = s;
                        }
                        match reading {
                            Some((measurement, pm_numbers)) => {
                                idle_cycles = 0;
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
                            None => idle_cycles += 1,
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
                        // The transaction was cut off part-way: don't reuse the bus as it is
                        rebuild_bus = true;
                    }
                }

                let reinit = if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    warn!("[sensor {}] {} too many errors; re-initializing", idx, name);
                    true
                } else if idle_cycles >= MAX_IDLE_CYCLES {
                    // It answers but isn't measuring: it lost power and is back in Idle mode
                    SENSOR_ERRORS[idx].fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "[sensor {}] {} no new data for {} s; re-initializing",
                        idx, name, idle_cycles
                    );
                    true
                } else {
                    false
                };
                if reinit {
                    running = false;
                    SENSOR_ONLINE[idx].store(false, Ordering::Relaxed);
                    rebuild_bus = true;
                }
            }

            ticker.next().await;
            if rebuild_bus {
                break;
            }
        }
    }
}
