# air-quality

Two Sensirion SEN55 air quality sensors, one before (`inlet`) and one after (`outlet`) an air
filter, on a Raspberry Pi Pico W (RP2040 + CYW43439), in Rust with embassy. Serves readings as
JSON over WiFi.

Sensor interface summary: [doc/sen55_interface.md](doc/sen55_interface.md)

## Architecture

| Core | Tasks |
|---|---|
| 0 | WiFi (cyw43 on PIO0 + DMA_CH0), embassy-net, picoserve web server, MCU temp, watchdog |
| 1 | `sensor0_task` (inlet, I2C0) and `sensor1_task` (outlet, I2C1), 1 Hz each |

The SEN55's I2C address (0x69) is fixed, so each sensor gets its own hardware I2C bus. Both tasks
run the same generic loop (`sensor_task::run`).

Each task publishes its readings to `SENSOR_DATA[i]` (an embassy-sync `Watch` over a
critical-section mutex, i.e. the RP2040 hardware spinlock). The web handlers on Core 0 read the
latest value with `try_get()`. Each task bumps `health::SENSOR_LOOPS[i]` every loop; the watchdog
on Core 0 only feeds while every counter has moved in the last 3 s, so a hang on either core (or
in either sensor task) reboots the board.

A sensor is reset and restarted automatically after 5 consecutive failed reads, so a loose
cable recovers without a reboot. A missing sensor doesn't affect the other one.

## Wiring

| SEN55 pin | #0 inlet (I2C0) | #1 outlet (I2C1) |
|---|---|---|
| 1 VDD | VBUS (5 V, pin 40) | VBUS (5 V, pin 40) |
| 2 GND | GND (pin 38) | GND (pin 38) |
| 3 SDA | GP4 (pin 6) | GP6 (pin 9) |
| 4 SCL | GP5 (pin 7) | GP7 (pin 10) |
| 5 SEL | GND | GND |
| 6 NC | — | — |

Each SDA and SCL line needs its own 10 kΩ pull-up to 3V3 (pin 36), four resistors in total.

- Each SEN55 needs 5 V at 63 mA average, up to 110 mA peak. Two sensors plus the Pico W fit
  within a 500 mA USB supply. VSYS sits a diode drop below VBUS, so take 5 V from VBUS.
- Pull-ups go to **3.3 V**, not 5 V. The SEN55 I/O accepts 3.3 V logic, so no level shifter
  is needed.
- Keep I2C wires under 10 cm or use shielded cable.
- The SEN55's metal shield is connected to GND internally. Don't connect the shield to anything
  at a different potential.

## Build & flash

WiFi credentials and the static IP are compiled in from environment variables:

```bash
export SSID=... PASSWORD=... IP=192.168.1.50   # /24, gateway x.x.x.1
cargo build
cargo embed          # flash + RTT log via debug probe (Embed.toml)
cargo run            # or: probe-rs run (runner in .cargo/config.toml)
```

## Web API

CORS is open (`*`). All values are JSON; unknown/invalid sensor values are `null`.

The device has no wall-clock time. The polling service should timestamp each reading
as `its own clock - age_secs`.

### `GET /api/air`
One entry per sensor, in index order (`inlet`, then `outlet`):
```json
{
  "sensors": [
    { "name": "inlet", "age_secs": 0, "reading": {
    "seq": 123, "uptime_secs": 130, "measuring_secs": 125,
    "measurement": {
      "pm1_0": 3.1, "pm2_5": 4.2, "pm4_0": 4.6, "pm10": 4.8,
      "humidity": 41.25, "temperature": 22.48,
      "voc_index": 98.0, "nox_index": 1.0
    },
    "pm_numbers": {
      "nc0_5": 21.3, "nc1_0": 25.0, "nc2_5": 25.2, "nc4_0": 25.3, "nc10": 25.3,
      "typical_size_um": 0.512
    },
    "status": {
      "raw": 0, "fan_speed_warning": false, "fan_cleaning": false,
      "gas_error": false, "rht_error": false, "laser_error": false, "fan_error": false
    } } },
    { "name": "outlet", "age_secs": 0, "reading": { "...": "same shape" } }
  ]
}
```
A sensor's `reading` is `null` until its first measurement arrives. Use `measuring_secs` to judge warm-up:
PM is stable after ~30 s, the VOC Index meets spec after ~1 h, the NOx Index after ~6 h.
(Example values above are illustrative.)

### `GET /api/status`
```json
{
  "uptime_secs": 130, "mcu_temp_c": 27.4,
  "last_reset": "power_on",
  "sensors": [
    { "name": "inlet",  "online": true, "errors": 0,
      "info": { "product": "SEN55", "serial": "...", "firmware": 2 } },
    { "name": "outlet", "online": true, "errors": 0, "info": { "...": "..." } }
  ]
}
```

## Measuring a filter

Filter efficiency is `1 - outlet / inlet`. Compute it on the client from averaged readings, not
from single 1 s samples:

- **Calibrate the pair first.** Run both sensors side by side in the same air for a while
  without the filter and record the outlet/inlet ratio. Device-to-device variation is
  ±5 µg/m³ + 5 % for PM1/PM2.5 below 100 µg/m³, which is large compared with a good filter's
  residual.
- **Efficiency is meaningless at low inlet concentrations.** With inlet PM2.5 near 5 µg/m³
  the result is mostly sensor noise. Use a particle source (e.g. a match or incense, away from
  the sensors) or only compute it when the inlet reading is high.
- **Prefer number concentrations for fine particles.** `nc0_5` and `nc1_0` count particles
  directly. Mass is dominated by the few large particles, while number shows how well the
  filter stops small ones.
- **PM4 and PM10 are estimated** from the size distribution, not measured. Don't use them for
  efficiency.
- **VOC and NOx Indices are relative to each sensor's own 24 h history.** Each sensor adapts
  to the air it sees, so comparing inlet and outlet indices doesn't measure gas removal.
  For carbon filters, compare raw VOC/NOx ticks (0x03D2, not yet implemented) instead.

## Todo

- [ ] Hardware test (only compile-checked so far)
- [ ] Persist the VOC algorithm state (0x6181) across reboots so the VOC Index doesn't relearn for an hour
- [ ] Temperature compensation / RH/T acceleration mode once mounted in an enclosure
- [ ] POST endpoints (e.g. start fan cleaning, clear status)
- [ ] Web client
