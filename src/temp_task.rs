//! Temperature Task
//!
//! Reads the RP2040 internal temperature sensor (ADC4) and publishes it in
//! TEMP_DECI_C for the web status.
//!
//! Sampling:
//! - Reads the sensor every 250ms
//! - Publishes the average of 8 samples every 2 seconds
//! - Logs the temperature once a minute

use defmt::{info, warn};
use embassy_rp::adc::{Adc, Async, Channel};
use embassy_time::{Duration, Ticker};
use portable_atomic::Ordering;

use crate::TEMP_DECI_C;

/// Samples averaged per published reading
const SAMPLES_PER_READING: u32 = 8;

/// Published readings between log lines (30 × 2s = 1 minute)
const READINGS_PER_LOG: u32 = 30;

/// Convert raw ADC reading to temperature in 0.1°C units
/// Formula from RP2040 datasheet: T = 27 - (V - 0.706) / 0.001721
/// where V = raw * 3.3 / 4096
#[inline]
fn convert_to_temp_scaled(raw: u16) -> i16 {
    // ADC reference voltage is 3.3V, 12-bit resolution (4096 levels)
    // Using fixed-point math to avoid floats
    // V_scaled = raw * 3300 / 4096 (in millivolts)
    // T = 27 - (V - 706mV) / 1.721mV/°C
    // T * 10 = 270 - (V_mv - 706) * 10 / 1.721
    // T * 10 = 270 - (V_mv - 706) * 5.81
    // Using integer approximation: * 581 / 100
    let v_mv = (raw as i32 * 3300) / 4096;
    let temp_scaled = 270 - ((v_mv - 706) * 581) / 100;
    temp_scaled as i16
}

/// Temperature task - samples the internal sensor and publishes an averaged reading.
#[embassy_executor::task]
pub async fn temp_task(mut adc: Adc<'static, Async>, mut temp_sensor: Channel<'static>) -> ! {
    let mut ticker = Ticker::every(Duration::from_millis(250));
    let mut sum: u32 = 0;
    let mut samples: u32 = 0;
    let mut readings: u32 = 0;

    loop {
        ticker.next().await;

        match adc.read(&mut temp_sensor).await {
            Ok(raw) => {
                sum += raw as u32;
                samples += 1;
            }
            Err(_) => {
                warn!("[temp] ADC read failed");
                continue;
            }
        }

        if samples < SAMPLES_PER_READING {
            continue;
        }

        let temp = convert_to_temp_scaled((sum / samples) as u16);
        TEMP_DECI_C.store(temp, Ordering::Relaxed);
        sum = 0;
        samples = 0;

        readings += 1;
        if readings >= READINGS_PER_LOG {
            readings = 0;
            let sign = if temp < 0 { "-" } else { "" };
            let abs = temp.unsigned_abs();
            info!("[temp] {}{}.{}C", sign, abs / 10, abs % 10);
        }
    }
}
