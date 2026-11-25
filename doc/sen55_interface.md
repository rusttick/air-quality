# SEN55 — Electrical & Data Interface Summary

Condensed from the Sensirion docs in [`ref/`](ref/). Primary source is the
datasheet (`ref/Sensirion_Datasheet_Environmental_Node_SEN5x.pdf`, v2 March 2022);
other sources noted inline.

## 1. Electrical

### Connector & pinout
Sensor side: ACES 51451-0060N-001 (6-pin, 1.25 mm). Mating plug: ACES 51452-006H0H0-001
or JST GHR-06V-S.

| Pin | Name | Connect to |
|----|------|-----------|
| 1 | VDD | 5 V ±10% |
| 2 | GND | Ground |
| 3 | SDA | I²C data (open drain, 3.3 V-logic compatible) |
| 4 | SCL | I²C clock (open drain, 3.3 V-logic compatible) |
| 5 | SEL | **GND** (selects I²C; must be low at or before power-up) |
| 6 | NC | Leave unconnected |

### Power
| Parameter | Value |
|---|---|
| Supply | 4.5 – 5.5 V (5.0 typ) |
| Startup until I²C ready | ≤ 50 ms |
| Idle | 3.8 mA first 10 s, then 2.6 mA typ (3 mA max) |
| RHT/Gas-only mode | 6.8 mA typ (8 mA max) |
| Measurement mode | 70 mA typ (100 mA max) for first 60 s, then 63 mA typ (80 mA max) |
| Peak | 110 mA max (16 µs pulses) |

Sensirion recommends a constant supply voltage for optimal performance. Size the
supply for the 110 mA peak.

### I/O levels
- VIH ≥ 2.31 V, VIL ≤ 0.99 V, VOL ≤ 0.4 V @ < 6 mA sink. I/O pins tolerate up to 5.5 V.
- A 3.3 V MCU can drive the bus directly with pull-ups to 3.3 V. **No level shifter is needed.**
- Use external pull-ups on SDA and SCL (the datasheet example uses 10 kΩ).
- Keep I²C wires < 10 cm, or use shielded cable.

### Gotchas
- **GND is internally connected to the metal shield.** Leave the shield floating, or
  make sure the shield and GND are at the same potential. Current through the shield
  can damage the part.
- ESD-sensitive (±4 kV contact). Handle accordingly.

## 2. I²C protocol

| | |
|---|---|
| Address | **0x69** (7-bit) |
| Speed | Standard mode, **100 kbit/s max** |
| Clock stretching | Not used |
| Command | 16-bit, big-endian; no CRC appended to the command itself |
| Data | 16-bit words, big-endian, **each followed by a CRC-8** |

**CRC-8:** polynomial 0x31, init 0xFF, no reflection, final XOR 0x00.
Check value: CRC(0xBEEF) = 0x92.

**Transactions:**
- **Write command:** `[cmd_hi cmd_lo]`, then wait for the execution time.
- **Write with parameters:** `[cmd_hi cmd_lo d0 d1 crc d2 d3 crc …]`
- **Read:** write `[cmd_hi cmd_lo]`, wait for the execution time (≥ 20 ms for reads),
  then read N×3 bytes and verify each CRC.

All settings are **volatile**: a reset or power cycle restores the defaults, so
reconfigure after every boot.

### Operating modes
```
Power-on/Reset ─► Idle ──0x0021──► Measurement (PM+RHT+VOC+NOx, 1 Hz)
                   │  ◄──0x0104──┘
                   └──0x0037──► RHT/Gas-only (no fan/laser, PM = 0xFFFF, 1 Hz)
```
Switching directly between Measurement and RHT/Gas-only requires firmware ≥ v2.
Otherwise, go through Idle.

### Command set
| Cmd | Name | Exec time | Notes |
|---|---|---|---|
| 0x0021 | Start Measurement | 50 ms | |
| 0x0037 | Start RHT/Gas-only Measurement | 50 ms | Low-power mode, no PM |
| 0x0104 | Stop Measurement | 200 ms | Returns to Idle |
| 0x0202 | Read Data-Ready | 20 ms | Byte 1: 0x01 = new data |
| 0x03C4 | Read Measured Values | 20 ms | Main output, see §3 |
| 0x0413 | Read Measured PM Values | 20 ms | Mass + number concentrations + particle size, FW > 0.7 ¹ |
| 0x03D2 | Read Raw Values | 20 ms | Raw RH/T and raw VOC/NOx ticks ² |
| 0x60B2 | R/W Temperature Compensation | 20 ms | Offset, slope, time constant |
| 0x60C6 | R/W Warm Start | 20 ms | 0 = cold (default) … 65535 = warm |
| 0x60D0 | R/W VOC Algorithm Tuning | 20 ms | Idle mode only |
| 0x60E1 | R/W NOx Algorithm Tuning | 20 ms | Idle mode only |
| 0x60F7 | R/W RH/T Acceleration Mode | 20 ms | 0 = low (default), 1 = high, 2 = medium |
| 0x6181 | R/W VOC Algorithm State | 20 ms | Back up and restore to skip relearning |
| 0x5607 | Start Fan Cleaning | 20 ms | Measurement mode only, takes 10 s |
| 0x8004 | R/W Auto-Cleaning Interval | 20 ms | u32 seconds, default 604800 (1 week), 0 = off |
| 0xD014 | Read Product Name | 20 ms | 32-char ASCII |
| 0xD033 | Read Serial Number | 20 ms | 32-char ASCII |
| 0xD100 | Read Firmware Version | 20 ms | |
| 0xD206 | Read Device Status | 20 ms | u32, see below |
| 0xD210 | Clear Device Status | 20 ms | |
| 0xD304 | Device Reset | 100 ms | |

¹ `ref/Sensirion_SEN5x_Read_Mass_and_Number_Concentrations.pdf` (not in the datasheet)
² `ref/PS_AN_Read_RHT_VOC_and_NOx_RAW_signals_v2_D1.pdf` (not in the datasheet)

### Device Status Register (0xD206)
| Bit | Name | Meaning | Auto-clears? |
|---|---|---|---|
| 21 | SPEED | Fan speed out of range (warning) | Yes |
| 19 | FAN | Fan cleaning active (info) | Yes |
| 7 | GAS | VOC/NOx sensor error | — |
| 6 | RHT | Internal RHT communication error | — |
| 5 | LASER | Laser current out of range | **No** |
| 4 | FAN | Fan blocked or broken (0 RPM) | **No** |

Ignore all reserved bits. A SPEED bit that stays set points to a supply problem or a
failing fan.

## 3. Data output

### Read Measured Values (0x03C4): 8 words, 24 bytes
| Word | Type | Scale | Quantity | Unknown value |
|---|---|---|---|---|
| 0 | u16 | ÷10 | PM1.0 [µg/m³] | 0xFFFF |
| 1 | u16 | ÷10 | PM2.5 [µg/m³] | 0xFFFF |
| 2 | u16 | ÷10 | PM4.0 [µg/m³] | 0xFFFF |
| 3 | u16 | ÷10 | PM10 [µg/m³] | 0xFFFF |
| 4 | i16 | ÷100 | Humidity [%RH] | 0x7FFF |
| 5 | i16 | ÷200 | Temperature [°C] | 0x7FFF |
| 6 | i16 | ÷10 | VOC Index | 0x7FFF |
| 7 | i16 | ÷10 | NOx Index | 0x7FFF |

The datasheet says "0xFFFF if unknown" for all values. The raw-values addendum and
Sensirion's drivers use 0x7FFF for the signed fields. Treat both as invalid.

Reading these values clears the data-ready flag. Without new data, the sensor returns
the previous values again.

### Read Measured PM Values (0x0413): 10 words, 30 bytes, all u16
PM1.0, PM2.5, PM4.0, PM10 mass [µg/m³, ÷10]; then PM0.5, PM1.0, PM2.5, PM4.0, PM10
**number** concentrations [#/cm³, ÷10]; then typical particle size [µm, ÷1000].

### Read Raw Values (0x03D2): 4 words
Raw RH (i16, ÷100), raw T (i16, ÷200), raw VOC (u16 ticks), raw NOx (u16 ticks).
Raw NOx reads 0xFFFF for the first 10–11 s. No specifications are given for raw values.

## 4. Interpreting the data

### Particulate matter
- Specified range: 0–1000 µg/m³. Particles 0.3–10 µm. Calibrated against a TSI
  DustTrak DRX 8533.
- Precision for PM1/PM2.5: ±(5 µg/m³ + 5%) below 100 µg/m³, ±10% above.
- **PM4 and PM10 are estimated** from the measured size distribution, not measured
  directly. Their precision is ±25 µg/m³ / ±25%, so treat them as rough values.
- Start-up: 8 s at normal concentrations, up to 30 s in very clean air (50–100 #/cm³).
  Discard readings from that window.
- Values freeze for 10 s during fan cleaning (status bit 19).
- Spec definitions: `ref/Sensirion_Particulate_Matter_AppNotes_Specification_Statement.pdf`

### Temperature & humidity
- Accuracy: ±0.45 °C typ (±0.7 max) at 15–30 °C. ±4.5 %RH typ (±6 max) at 30–70 %RH.
- The default compensation assumes a **bare module in free air**. In an enclosure, tune
  the offset and slope (0x60B2), the acceleration mode (0x60F7) and warm start (0x60C6)
  against a reference. See
  `ref/Sensirion_Temperature_Acceleration_and_Compensation_Instructions_SEN.pdf`.

### VOC Index (1–500)
- **A relative value, not a concentration.** 100 is the average of the past 24 hours.
  Above 100 means more VOCs than usual; below 100 means fewer. The gain adapts over time.
- It reliably detects events after about 60 s, and meets spec after about 1 h.
- Stopping and restarting a measurement resets the learning. To resume, back up the
  algorithm state with 0x6181 and restore it before the next start.
- Device-to-device variation: ±15 points or ±15%.
- Don't convert it to ppb or tVOC. MOX sensors are broadband, so error can exceed a
  factor of 2. (`ref/Info_Note_Output_MOX_Sensor.pdf`)
- For RESET/WELL reporting only, see `ref/GAS_AN_SGP4x_BuildingStandards_D1_1.pdf`
  for the lab-conditions mapping from VOC Index to TVOC.

### NOx Index (1–500)
- 1 is the baseline (normally clean air). Values above 1 indicate an NOx event, e.g.
  a gas stove. The gain does not adapt.
- Meets spec after about 6 h. Device-to-device variation: ±50 points or ±50%.
- Not available in the first ~10 s after power-on.

## 5. Minimal bring-up sequence
1. Tie SEL to GND, apply 5 V, wait ≥ 50 ms.
2. `0xD304` reset, wait 100 ms.
3. Optional: read the product name, serial number and firmware version.
4. Optional (must happen before start): set the temperature compensation, RH/T
   acceleration mode and warm start; restore the VOC state.
5. `0x0021` start measurement, wait 50 ms.
6. Loop every 1 s: `0x0202` data-ready, then `0x03C4` read values.
   Check `0xD206` status periodically.
7. Before shutdown: back up the VOC state with `0x6181`.

To save power, alternate Measurement mode with RHT/Gas-only mode. This cuts average
current 7–9× and still allows for PM start-up time. See
`ref/Reduced_Power_Operation_SEN5x.pdf`.
