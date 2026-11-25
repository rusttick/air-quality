# Wiring a SEN55 over Cat6 (~10 ft / 3 m)

Use this to connect each SEN55 to the Pico W with a standard Cat6 patch cable: I²C, 5 V power and
ground all in one cable. The design is sized for about 10 ft (3 m), with plenty of margin.
Each sensor gets its own cable and its own I²C bus (inlet on I2C0, outlet on I2C1).

> ⚠️ **Use a straight-through cable only.** A crossover cable puts 5 V on SCL, which will damage
> the RP2040 (its GPIOs are not 5 V tolerant).
>
> ⚠️ **Never plug these jacks into Ethernet equipment.** A PoE switch can put up to 57 V on the
> pairs. Label both ends "I²C – NOT ETHERNET".

## Pair assignment (T568B colors)

Each I²C signal is twisted with its own ground, which keeps SDA and SCL from coupling into each
other. Power uses the remaining two pairs in parallel.

| RJ45 pin | Wire color | Signal | SEN55 pin |
|---|---|---|---|
| 1 | white/orange | GND | 2 |
| 2 | orange | **SCL** | 4 |
| 3 | white/green | GND | 2 |
| 4 | blue | **SDA** | 3 |
| 5 | white/blue | GND | 2 |
| 6 | green | **+5 V** | 1 |
| 7 | white/brown | GND | 2 |
| 8 | brown | **+5 V** | 1 |

All four white wires are GND. Both solid green and solid brown are +5 V.

## Schematic (one sensor)

```
 Pico W end                                             Sensor end
 ──────────                                             ──────────
 3V3 ──┬──────┐
      3.3k   3.3k
       │      │
 GPx ──┴─[100Ω]─────── RJ45 pin 4 (SDA) ──────[100Ω]── SEN55 pin 3 SDA
 GPy ─────┴─[100Ω]──── RJ45 pin 2 (SCL) ──────[100Ω]── SEN55 pin 4 SCL
 VBUS ──────────────── RJ45 pins 6, 8 (+5V) ────────┬── SEN55 pin 1 VDD
                                                    ═╪═ 10µF + 100nF
 GND ───────────────── RJ45 pins 1,3,5,7 (GND) ─────┴── SEN55 pin 2 GND
                                                        SEN55 pin 5 SEL ── GND (at sensor)
                                                        SEN55 pin 6 NC  ── not connected
```

| Sensor | Bus | SDA (GPx) | SCL (GPy) |
|---|---|---|---|
| inlet | I2C0 | GP4 (pin 6) | GP5 (pin 7) |
| outlet | I2C1 | GP6 (pin 9) | GP7 (pin 10) |

Shared on the Pico W: 3V3(OUT) pin 36 (pull-ups only), VBUS pin 40 (sensor power), GND (pins 38, 33, …).

## Components and why

| Part | Where | Value | Why |
|---|---|---|---|
| Pull-ups | Pico end, SDA and SCL to **3.3 V** | 3.3 kΩ (4.7 kΩ also fine) | ~3 m of Cat6 is roughly 150–200 pF per line. 3.3 kΩ gives a ~0.5 µs rise, well within spec even at 100 kHz. Never pull up to 5 V. |
| Series resistors | Both ends, on SDA and SCL | 100 Ω | Limit the current when a driver discharges the cable (the SEN55 pins are rated ±16 mA max), and damp ringing. They add only ~0.2 V to the low level the Pico sees. |
| Decoupling | Sensor end, VDD to GND | 10 µF + 100 nF | Absorbs the fan's and laser's 110 mA current pulses locally, so they don't bounce the ground. |
| SEL | Sensor end | Tie to GND | Selects I²C. Don't run it down the cable. |

**Power drop:** 24 AWG conductors with two in parallel each way give about 0.25 Ω round trip over
3 m. That's about 30 mV at the 110 mA peak, which is negligible.

## Hardware

- **Sensor end:** a small adapter board with an RJ45 jack, the 6-pin JST GH connector (or a
  pre-crimped JST GH 6-pin pigtail to the SEN55), two 100 Ω resistors, and the 10 µF + 100 nF
  capacitors.
- **Pico end:** an RJ45 jack per sensor, the pull-ups, and the series resistors.
- **Cable:** any straight-through Cat5e or Cat6 patch cable (UTP is fine at this length).
  Shielded (STP) isn't needed at 3 m. If you use it, connect the shield to GND at the Pico end only.

Instead of jacks, you can also cut a patch cable and solder it directly. The pair assignment is
the same.

## Firmware settings

These are set in `src/sensor_task.rs`:

- **I²C clock:** `I2C_FREQUENCY = 10_000` (10 kHz). At 3 m, 100 kHz also works with these
  parts, but 10 kHz gives a large margin for free. One read cycle takes about 150 ms at 10 kHz,
  well within the 1 s period and the 500 ms read timeout.
- **Internal pull-ups off:** embassy-rp's `i2c::Config::default()` enables the RP2040's internal
  pull-ups on SDA and SCL. With external 3.3 kΩ pull-ups, set `config.sda_pullup = false` and
  `config.scl_pullup = false`.
- **Recovery:** the firmware restarts a sensor after 5 failed read cycles in a row, or after
  30 s without new data, so a sensor can be unplugged and replugged while the Pico is running.
  Before each start it frees the bus if the sensor is holding SDA low. This relies on the
  external pull-ups.

## Bring-up checks

1. With the cable connected but the sensor unplugged, check continuity on all 8 pins end to end.
   Confirm pin 6 and pin 8 (+5 V) are **not** connected to pin 2 or pin 4.
2. Power up without the sensor. Measure about 5 V between pins 6/8 and 1/3/5/7 at the sensor end,
   and about 3.3 V on pins 2 and 4 (the pull-ups) at the sensor end.
3. Plug in the sensor. Within a few seconds the log should show
   `[sensor 0] inlet: SEN55 serial … measuring`, and `/api/status` should report
   `"online": true`. `"errors"` counted one failed start per second while the sensor was
   unplugged; it should now stop rising.
4. Optional, with a scope: the SCL low level at the Pico is under 0.8 V, and the edges are clean
   with no double steps.

If the log shows `I2C bus is stuck low (check wiring and pull-ups)`, SDA or SCL reads low
while idle: a pull-up is missing, or the line is shorted to GND.
