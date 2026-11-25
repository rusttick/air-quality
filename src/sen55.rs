//! Minimal async driver for the Sensirion SEN55 over I2C
//!
//! Written against `doc/sen55_interface.md` (datasheet v2, March 2022, plus the
//! 0x0413 "Read Measured PM Values" addendum). Generic over any
//! `embedded_hal_async::i2c::I2c`, uses `embassy_time` for command delays.
//!
//! Protocol summary:
//! - 7-bit address 0x69, standard mode (100 kHz max), no clock stretching
//! - 16-bit big-endian command, no CRC on the command itself
//! - data is 16-bit big-endian words, each followed by CRC-8 (poly 0x31, init 0xFF)
//! - reads: write command, wait the execution time, then read N x 3 bytes

use embassy_time::Timer;
use embedded_hal_async::i2c::I2c;

/// SEN5x I2C address (fixed)
pub const ADDR: u8 = 0x69;

mod cmd {
    pub const START_MEASUREMENT: u16 = 0x0021;
    pub const START_RHT_GAS_ONLY: u16 = 0x0037;
    pub const STOP_MEASUREMENT: u16 = 0x0104;
    pub const READ_DATA_READY: u16 = 0x0202;
    pub const READ_MEASURED_VALUES: u16 = 0x03C4;
    pub const READ_MEASURED_PM_VALUES: u16 = 0x0413;
    pub const VOC_ALGORITHM_STATE: u16 = 0x6181;
    pub const START_FAN_CLEANING: u16 = 0x5607;
    pub const READ_PRODUCT_NAME: u16 = 0xD014;
    pub const READ_SERIAL_NUMBER: u16 = 0xD033;
    pub const READ_FIRMWARE_VERSION: u16 = 0xD100;
    pub const READ_DEVICE_STATUS: u16 = 0xD206;
    pub const CLEAR_DEVICE_STATUS: u16 = 0xD210;
    pub const RESET: u16 = 0xD304;
}

/// Sentinel the sensor returns for unknown u16 values
const UNKNOWN_U16: u16 = 0xFFFF;
/// Sentinel for unknown i16 values (datasheet says 0xFFFF for all fields, but the
/// raw-values addendum and Sensirion's drivers use 0x7FFF for signed ones)
const UNKNOWN_I16: i16 = 0x7FFF;

/// Largest read: product name / serial number = 16 words x 3 bytes
const MAX_READ: usize = 48;
/// Largest write: command + 4 words (VOC algorithm state)
const MAX_WRITE: usize = 2 + 4 * 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum Error<E> {
    /// Underlying I2C bus error (NACK, arbitration, ...)
    I2c(E),
    /// CRC mismatch on a received word
    Crc,
}

/// CRC-8 as specified in the datasheet: poly 0x31, init 0xFF, no reflection, no final XOR
pub const fn crc8(data: [u8; 2]) -> u8 {
    let mut crc: u8 = 0xFF;
    let mut i = 0;
    while i < 2 {
        crc ^= data[i];
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x31 } else { crc << 1 };
            bit += 1;
        }
        i += 1;
    }
    crc
}

// Datasheet example: CRC(0xBEEF) = 0x92 (checked at compile time)
const _: () = assert!(crc8([0xBE, 0xEF]) == 0x92);

/// Output of Read Measured Values (0x03C4), scaled to physical units.
/// `None` means the sensor reported the value as unknown (e.g. PM in RHT/Gas-only
/// mode, or NOx during the first ~10 s).
#[derive(Debug, Clone, Copy, Default, PartialEq, defmt::Format, serde::Serialize)]
pub struct Measurement {
    /// PM1.0 mass concentration [µg/m³]
    pub pm1_0: Option<f64>,
    /// PM2.5 mass concentration [µg/m³]
    pub pm2_5: Option<f64>,
    /// PM4.0 mass concentration [µg/m³] (estimated from size distribution)
    pub pm4_0: Option<f64>,
    /// PM10 mass concentration [µg/m³] (estimated from size distribution)
    pub pm10: Option<f64>,
    /// Compensated ambient relative humidity [%RH]
    pub humidity: Option<f64>,
    /// Compensated ambient temperature [°C]
    pub temperature: Option<f64>,
    /// VOC Index, 1..500, 100 = 24 h average (relative, not a concentration)
    pub voc_index: Option<f64>,
    /// NOx Index, 1..500, 1 = baseline (relative, not a concentration)
    pub nox_index: Option<f64>,
}

/// Particle number concentrations and typical size from Read Measured PM Values (0x0413).
/// Requires firmware > 0.7.
#[derive(Debug, Clone, Copy, Default, PartialEq, defmt::Format, serde::Serialize)]
pub struct PmNumbers {
    /// Number concentration PM0.5 [#/cm³]
    pub nc0_5: Option<f64>,
    /// Number concentration PM1.0 [#/cm³]
    pub nc1_0: Option<f64>,
    /// Number concentration PM2.5 [#/cm³]
    pub nc2_5: Option<f64>,
    /// Number concentration PM4.0 [#/cm³]
    pub nc4_0: Option<f64>,
    /// Number concentration PM10 [#/cm³]
    pub nc10: Option<f64>,
    /// Typical particle size [µm]
    pub typical_size_um: Option<f64>,
}

/// Device Status Register (0xD206), decoded. Reserved bits are ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, defmt::Format, serde::Serialize)]
pub struct DeviceStatus {
    /// Raw 32-bit register value
    pub raw: u32,
    /// Bit 21: fan speed out of range (warning, auto-clears)
    pub fan_speed_warning: bool,
    /// Bit 19: fan cleaning in progress (info; values frozen for 10 s)
    pub fan_cleaning: bool,
    /// Bit 7: VOC/NOx gas sensor error
    pub gas_error: bool,
    /// Bit 6: internal RH/T sensor communication error
    pub rht_error: bool,
    /// Bit 5: laser current out of range (sticky)
    pub laser_error: bool,
    /// Bit 4: fan blocked or broken (sticky)
    pub fan_error: bool,
}

impl DeviceStatus {
    pub fn from_raw(raw: u32) -> Self {
        let bit = |n: u32| raw & (1 << n) != 0;
        Self {
            raw,
            fan_speed_warning: bit(21),
            fan_cleaning: bit(19),
            gas_error: bit(7),
            rht_error: bit(6),
            laser_error: bit(5),
            fan_error: bit(4),
        }
    }

    /// True if any error bit (not warning/info) is set
    pub fn has_error(&self) -> bool {
        self.gas_error || self.rht_error || self.laser_error || self.fan_error
    }
}

fn scale_u16(v: u16, div: f64) -> Option<f64> {
    (v != UNKNOWN_U16).then(|| v as f64 / div)
}

fn scale_i16(v: u16, div: f64) -> Option<f64> {
    let v = v as i16;
    // Only 0x7FFF: a signed 0xFFFF (-1) is a valid reading, e.g. -0.005 °C
    (v != UNKNOWN_I16).then(|| v as f64 / div)
}

pub struct Sen55<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Sen55<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    // ------------------------------------------------------------------
    // Low-level framing
    // ------------------------------------------------------------------

    /// Send a command with no data, then wait its execution time
    async fn write_cmd(&mut self, cmd: u16, exec_ms: u64) -> Result<(), Error<I2C::Error>> {
        self.i2c
            .write(ADDR, &cmd.to_be_bytes())
            .await
            .map_err(Error::I2c)?;
        Timer::after_millis(exec_ms).await;
        Ok(())
    }

    /// Send a command followed by data words (each with CRC), then wait
    async fn write_words(
        &mut self,
        cmd: u16,
        words: &[u16],
        exec_ms: u64,
    ) -> Result<(), Error<I2C::Error>> {
        debug_assert!(words.len() <= 4);
        let mut buf = [0u8; MAX_WRITE];
        buf[..2].copy_from_slice(&cmd.to_be_bytes());
        let mut n = 2;
        for w in words {
            let b = w.to_be_bytes();
            buf[n] = b[0];
            buf[n + 1] = b[1];
            buf[n + 2] = crc8(b);
            n += 3;
        }
        self.i2c.write(ADDR, &buf[..n]).await.map_err(Error::I2c)?;
        Timer::after_millis(exec_ms).await;
        Ok(())
    }

    /// Send a command, wait, read N words and verify each CRC
    async fn read_words<const N: usize>(
        &mut self,
        cmd: u16,
        exec_ms: u64,
    ) -> Result<[u16; N], Error<I2C::Error>> {
        let mut buf = [0u8; MAX_READ];
        let len = N * 3;
        debug_assert!(len <= MAX_READ);

        self.write_cmd(cmd, exec_ms).await?;
        self.i2c
            .read(ADDR, &mut buf[..len])
            .await
            .map_err(Error::I2c)?;

        let mut out = [0u16; N];
        for (i, chunk) in buf[..len].chunks_exact(3).enumerate() {
            let data = [chunk[0], chunk[1]];
            if crc8(data) != chunk[2] {
                return Err(Error::Crc);
            }
            out[i] = u16::from_be_bytes(data);
        }
        Ok(out)
    }

    /// Read a 32-character ASCII string (product name / serial number)
    async fn read_string(&mut self, cmd: u16) -> Result<heapless::String<32>, Error<I2C::Error>> {
        let words: [u16; 16] = self.read_words(cmd, 20).await?;
        let mut s = heapless::String::new();
        for w in words {
            for b in w.to_be_bytes() {
                if b == 0 {
                    return Ok(s);
                }
                // ASCII only; skip anything unexpected
                if b.is_ascii() {
                    let _ = s.push(b as char);
                }
            }
        }
        Ok(s)
    }

    // ------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------

    /// Software reset (0xD304); module returns to Idle with default settings
    pub async fn reset(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::RESET, 100).await
    }

    /// Start full measurement: PM + RH/T + VOC + NOx, new data every second (0x0021)
    pub async fn start_measurement(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::START_MEASUREMENT, 50).await
    }

    /// Start RHT/Gas-only measurement: fan and laser off, PM unknown (0x0037)
    pub async fn start_rht_gas_only(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::START_RHT_GAS_ONLY, 50).await
    }

    /// Stop measurement, return to Idle (0x0104)
    pub async fn stop_measurement(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::STOP_MEASUREMENT, 200).await
    }

    /// True if a new measurement is available since the last read (0x0202)
    pub async fn data_ready(&mut self) -> Result<bool, Error<I2C::Error>> {
        let [w]: [u16; 1] = self.read_words(cmd::READ_DATA_READY, 20).await?;
        Ok(w & 0x00FF == 0x01)
    }

    /// Read the latest measured values and clear the data-ready flag (0x03C4)
    pub async fn read_measurement(&mut self) -> Result<Measurement, Error<I2C::Error>> {
        let w: [u16; 8] = self.read_words(cmd::READ_MEASURED_VALUES, 20).await?;
        Ok(Measurement {
            pm1_0: scale_u16(w[0], 10.0),
            pm2_5: scale_u16(w[1], 10.0),
            pm4_0: scale_u16(w[2], 10.0),
            pm10: scale_u16(w[3], 10.0),
            humidity: scale_i16(w[4], 100.0),
            temperature: scale_i16(w[5], 200.0),
            voc_index: scale_i16(w[6], 10.0),
            nox_index: scale_i16(w[7], 10.0),
        })
    }

    /// Read particle number concentrations and typical particle size (0x0413).
    /// The mass concentrations in this response duplicate `read_measurement` and are skipped.
    pub async fn read_pm_numbers(&mut self) -> Result<PmNumbers, Error<I2C::Error>> {
        let w: [u16; 10] = self.read_words(cmd::READ_MEASURED_PM_VALUES, 20).await?;
        Ok(PmNumbers {
            nc0_5: scale_u16(w[4], 10.0),
            nc1_0: scale_u16(w[5], 10.0),
            nc2_5: scale_u16(w[6], 10.0),
            nc4_0: scale_u16(w[7], 10.0),
            nc10: scale_u16(w[8], 10.0),
            typical_size_um: scale_u16(w[9], 1000.0),
        })
    }

    /// Read the device status register (0xD206)
    pub async fn read_status(&mut self) -> Result<DeviceStatus, Error<I2C::Error>> {
        let [hi, lo]: [u16; 2] = self.read_words(cmd::READ_DEVICE_STATUS, 20).await?;
        Ok(DeviceStatus::from_raw(((hi as u32) << 16) | lo as u32))
    }

    /// Clear all device status flags, including sticky errors (0xD210)
    pub async fn clear_status(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::CLEAR_DEVICE_STATUS, 20).await
    }

    /// Start a manual 10 s fan cleaning; only valid in Measurement mode (0x5607)
    pub async fn start_fan_cleaning(&mut self) -> Result<(), Error<I2C::Error>> {
        self.write_cmd(cmd::START_FAN_CLEANING, 20).await
    }

    /// Back up the VOC algorithm state (8 bytes) to skip relearning after a restart (0x6181)
    pub async fn read_voc_state(&mut self) -> Result<[u16; 4], Error<I2C::Error>> {
        self.read_words(cmd::VOC_ALGORITHM_STATE, 20).await
    }

    /// Restore a VOC algorithm state; write before `start_measurement` (0x6181)
    pub async fn write_voc_state(&mut self, state: [u16; 4]) -> Result<(), Error<I2C::Error>> {
        self.write_words(cmd::VOC_ALGORITHM_STATE, &state, 20).await
    }

    /// Product name, e.g. "SEN55" (0xD014)
    pub async fn product_name(&mut self) -> Result<heapless::String<32>, Error<I2C::Error>> {
        self.read_string(cmd::READ_PRODUCT_NAME).await
    }

    /// Serial number string (0xD033)
    pub async fn serial_number(&mut self) -> Result<heapless::String<32>, Error<I2C::Error>> {
        self.read_string(cmd::READ_SERIAL_NUMBER).await
    }

    /// Firmware version (0xD100)
    pub async fn firmware_version(&mut self) -> Result<u8, Error<I2C::Error>> {
        let [w]: [u16; 1] = self.read_words(cmd::READ_FIRMWARE_VERSION, 20).await?;
        Ok((w >> 8) as u8)
    }
}
