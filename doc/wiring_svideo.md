# Wiring a SEN55 over S-Video Cable (~10 ft / 3 m)

Use this to connect each SEN55 to the Pico W with an S-video cable for I²C, plus a separate
2-wire cable for 5 V power. The design is sized for about 10 ft (3 m), with plenty of margin.
Each sensor gets its own cables and its own I²C bus (inlet on I2C0, outlet on I2C1).

An S-video cable holds two 75 Ω mini-coax lines. Each I²C signal runs on a center conductor,
with its shield as the ground return, so SDA and SCL are shielded from each other and from
outside noise.

> ⚠️ **Label both ends "I²C – NOT VIDEO".** Plugging this into video equipment is unlikely to
> do damage, but it won't work, and the sensor's lines would be connected to a video output.

## Signal assignment

S-video uses 4-pin mini-DIN connectors. Standard pinout (looking at a female jack, or the solder
side of a male plug; check your connector's markings):

| Mini-DIN pin | S-video name | Signal | SEN55 pin |
|---|---|---|---|
| 1 | GND (Y) | GND, shield of SDA coax | 2 |
| 2 | GND (C) | GND, shield of SCL coax | 2 |
| 3 | Y (luma) | **SDA** | 3 |
| 4 | C (chroma) | **SCL** | 4 |

**Power cable (separate):** 2 conductors, 22 AWG or heavier: +5 V and GND.

Both coax shields and the power cable's GND are connected together at **both** ends. Together
they form the ground return. The shields carry the small I²C return currents; the power GND
carries the fan current.

## Schematic (one sensor)

```
 Pico W end                                                  Sensor end
 ──────────                                                  ──────────
 3V3 ──┬──────┐
      3.3k   3.3k
       │      │
 GPx ──┴─[100Ω]──── mini-DIN 3 ══ coax Y ══ mini-DIN 3 ──[100Ω]── SEN55 pin 3 SDA
 GND ─────────────── mini-DIN 1 ══ shield ══ mini-DIN 1 ────────┐
 GPy ─────┴─[100Ω]── mini-DIN 4 ══ coax C ══ mini-DIN 4 ──[100Ω]── SEN55 pin 4 SCL
 GND ─────────────── mini-DIN 2 ══ shield ══ mini-DIN 2 ────────┤
                                                                │
 VBUS ────────────── power +5V ───────────────────────────┬─────┼── SEN55 pin 1 VDD
                                                          ═╪═ 10µF + 100nF
 GND ─────────────── power GND ───────────────────────────┴─────┴── SEN55 pin 2 GND
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
| Pull-ups | Pico end, SDA and SCL to **3.3 V** | 3.3 kΩ (4.7 kΩ also fine) | Thin S-video coax is about 55–100 pF/m, so 3 m is roughly 200–300 pF per line. 3.3 kΩ gives a ~0.6–0.9 µs rise. That's fine at 10 kHz and borderline at 100 kHz. Never pull up to 5 V. |
| Series resistors | Both ends, on SDA and SCL | 100 Ω | Limit the current when a driver discharges the coax. Into 75 Ω, the initial current would otherwise approach the SEN55's ±16 mA pin limit. They also damp reflections on the unterminated line, and add only ~0.2 V to the low level the Pico sees. |
| Decoupling | Sensor end, VDD to GND | 10 µF + 100 nF | Absorbs the fan's and laser's 110 mA current pulses locally. |
| SEL | Sensor end | Tie to GND | Selects I²C. |

**Power drop:** 22 AWG over 3 m is about 0.33 Ω round trip, or about 36 mV at the 110 mA peak,
which is negligible. Anything from 18–24 AWG is fine at this length.

**Measure your cable (optional):** set a multimeter to capacitance, measure from center to
shield at one end, and leave the other end open. Under 400 pF per line means 100 kHz would also
work; 10 kHz works with far more.

## Hardware

- **Option A, connectors (recommended):** panel-mount or PCB 4-pin mini-DIN female jacks at both
  ends. Any off-the-shelf male-to-male S-video cable then plugs in, and S-video cables are
  wired straight through. The power cable can use a 2.1 mm barrel jack, screw terminals, or a
  2-pin JST connector.
- **Option B, soldered:** cut the connectors off and solder the coax directly. Strip carefully;
  the thin coax shield is easy to short to the center conductor. Check for a short with a meter
  before powering up.
- **Sensor end:** a small adapter board with the mini-DIN jack, power input, a 6-pin JST GH
  connector (or pre-crimped pigtail) to the SEN55, two 100 Ω resistors, and the 10 µF + 100 nF
  capacitors.

## Firmware settings

These are set in `src/sensor_task.rs`:

- **I²C clock:** `I2C_FREQUENCY = 10_000` (10 kHz). One read cycle takes about 150 ms at 10 kHz,
  well within the 1 s period and the 500 ms read timeout. At 10 kHz, embassy-rp also sets the
  RP2040's glitch filter to about 1.7 µs, which helps reject reflections on the coax.
- **Internal pull-ups off:** embassy-rp's `i2c::Config::default()` enables the RP2040's internal
  pull-ups on SDA and SCL. With external 3.3 kΩ pull-ups, set `config.sda_pullup = false` and
  `config.scl_pullup = false`.
- **Recovery:** the firmware restarts a sensor after 5 failed read cycles in a row, or after
  30 s without new data, so a sensor can be unplugged and replugged while the Pico is running.
  Before each start it frees the bus if the sensor is holding SDA low. This relies on the
  external pull-ups.

## Bring-up checks

1. With the cables connected but the sensor unplugged, check continuity end to end on pins 1–4
   and on the power pair. Confirm center and shield aren't shorted: pin 3 to pins 1/2, and
   pin 4 to pins 1/2.
2. Power up without the sensor. Measure about 5 V on the power cable at the sensor end, and
   about 3.3 V on mini-DIN pins 3 and 4 (the pull-ups) at the sensor end.
3. Plug in the sensor. Within a few seconds the log should show
   `[sensor 0] inlet: SEN55 serial … measuring`, and `/api/status` should report
   `"online": true`. `"errors"` counted one failed start per second while the sensor was
   unplugged; it should now stop rising.
4. Optional, with a scope: the SCL low level at the Pico is under 0.8 V, and the edges are clean
   with no double steps.

If the log shows `I2C bus is stuck low (check wiring and pull-ups)`, SDA or SCL reads low
while idle: a pull-up is missing, or the line is shorted to GND.
