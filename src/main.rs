//! RP2040 Air Quality Monitor (Multicore)
//!
//! Reads two Sensirion SEN55s (PM, RH/T, VOC, NOx), one before ("inlet") and
//! one after ("outlet") an air filter, on a Pico W and serves the readings as
//! JSON over HTTP.
//!
//! Multicore architecture:
//! - Core 0: WiFi, web server, NTP, MCU temperature, watchdog
//! - Core 1: one SEN55 task per I2C bus
//!
//! Core 1 publishes readings to `SENSOR_DATA[i]` (embassy-sync Watch, critical-section
//! mutex = RP2040 hardware spinlock), which the web handlers on Core 0 read.
//!
//! Hardware:
//! - GP4 / GP5: I2C0 SDA / SCL to SEN55 #0 "inlet" (Core 1)
//! - GP6 / GP7: I2C1 SDA / SCL to SEN55 #1 "outlet" (Core 1)
//! - CYW43: WiFi (PIO0 + DMA_CH0 on Core 0)
//!
//! Build-time environment: SSID, PASSWORD, IP (static address, /24, gateway x.x.x.1)

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::ptr::addr_of_mut;

use cyw43::JoinOptions;
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Executor;
use embassy_net::{Config as NetConfig, Ipv4Address, Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_rp::adc::{Adc, Channel, Config as AdcConfig};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{Stack, spawn_core1};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_rp::watchdog::Watchdog;
use embassy_time::{Duration, Timer};
use picoserve::{
    AppBuilder, AppRouter, make_static,
    response::{IntoResponse, Json},
    routing::get,
};
use portable_atomic::Ordering;
use static_cell::StaticCell;

use air_quality::health::{self, WifiState};
use air_quality::ntp;
use air_quality::sensor_task::{
    SENSOR_ERRORS, SENSOR_ONLINE, Sensor0Resources, Sensor1Resources, sensor0_task, sensor1_task,
};
use air_quality::temp_task::temp_task;
use air_quality::{
    Irqs, NUM_SENSORS, SENSOR_DATA, SENSOR_INFO, SENSOR_NAMES, SensorData, SensorInfo,
    TEMP_DECI_C, init_boot_time, unix_time, uptime_secs,
};

// WiFi credentials - set via environment variables at build time
const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

// Multicore statics
// Sensor tasks are small (I2C + a few structs); 16 KB leaves plenty of headroom
static mut CORE1_STACK: Stack<16384> = Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

/// On panic (either core): log it, mark it, and reboot via the watchdog
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    health::panic_reboot(info)
}

// ============================================================================
// Web API
// ============================================================================

/// CORS headers for cross-origin requests
const CORS_HEADERS: [(&str, &str); 3] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type"),
];

/// Per-sensor entry in GET /api/status
#[derive(serde::Serialize)]
struct SensorStatus {
    name: &'static str,
    online: bool,
    errors: u32,
    info: Option<SensorInfo>,
}

/// GET /api/status
#[derive(serde::Serialize)]
struct StatusResponse {
    uptime_secs: u64,
    time_unix: u64,
    /// RP2040 internal temperature [°C]
    mcu_temp_c: f64,
    last_reset: &'static str,
    sensors: [SensorStatus; NUM_SENSORS],
}

/// Per-sensor entry in GET /api/air
#[derive(serde::Serialize)]
struct SensorReading {
    name: &'static str,
    /// Seconds since `reading` was taken (None if no reading yet)
    age_secs: Option<u64>,
    reading: Option<SensorData>,
}

/// GET /api/air
#[derive(serde::Serialize)]
struct AirResponse {
    time_unix: u64,
    sensors: [SensorReading; NUM_SENSORS],
}

async fn status_handler() -> impl IntoResponse {
    let status = StatusResponse {
        uptime_secs: uptime_secs(),
        time_unix: unix_time(),
        mcu_temp_c: TEMP_DECI_C.load(Ordering::Relaxed) as f64 / 10.0,
        last_reset: health::reset_cause().as_str(),
        sensors: core::array::from_fn(|i| SensorStatus {
            name: SENSOR_NAMES[i],
            online: SENSOR_ONLINE[i].load(Ordering::Relaxed),
            errors: SENSOR_ERRORS[i].load(Ordering::Relaxed),
            info: SENSOR_INFO[i].try_get(),
        }),
    };
    Json(status).into_response().with_headers(CORS_HEADERS)
}

async fn air_handler() -> impl IntoResponse {
    let now = uptime_secs();
    let air = AirResponse {
        time_unix: unix_time(),
        sensors: core::array::from_fn(|i| {
            let reading = SENSOR_DATA[i].try_get();
            SensorReading {
                name: SENSOR_NAMES[i],
                age_secs: reading.as_ref().map(|r| now.saturating_sub(r.uptime_secs)),
                reading,
            }
        }),
    };
    Json(air).into_response().with_headers(CORS_HEADERS)
}

/// OPTIONS - CORS preflight handler
async fn options_handler() -> impl IntoResponse {
    (CORS_HEADERS, "")
}

struct WebApp;

impl AppBuilder for WebApp {
    type PathRouter = impl picoserve::routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        picoserve::Router::new()
            .route("/api/status", get(status_handler).options(options_handler))
            .route("/api/air", get(air_handler).options(options_handler))
    }
}

#[embassy_executor::task]
async fn web_task(
    stack: embassy_net::Stack<'static>,
    app: &'static AppRouter<WebApp>,
    config: &'static picoserve::Config<Duration>,
) -> ! {
    info!("[web] task starting");
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(app, config, &mut http_buffer)
        .listen_and_serve(0, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

/// Drives the CYW43 chip: SPI traffic, events, and the packet queue
#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

/// Drives the embassy-net (smoltcp) stack: timers, retransmits, ARP, sockets
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Join WiFi network, retrying until successful
/// Turns LED off while connecting, on when network stack is ready
async fn join_wifi(control: &mut cyw43::Control<'static>, stack: embassy_net::Stack<'static>) {
    control.gpio_set(0, false).await;
    health::set_wifi_state(WifiState::Joining);
    loop {
        info!("joining SSID = \"{}\"", SSID);
        match control
            .join(SSID, JoinOptions::new(PASSWORD.as_bytes()))
            .await
        {
            Ok(()) => {
                info!("connected to SSID = \"{}\"", SSID);
                stack.wait_config_up().await;
                control.gpio_set(0, true).await;
                health::set_wifi_state(WifiState::Connected);
                return;
            }
            Err(e) => {
                warn!("failed to join SSID = \"{}\": {}", SSID, e.status);
                health::set_wifi_state(WifiState::Failing);
                Timer::after_secs(1).await;
            }
        }
    }
}

/// Core 0 main task - handles WiFi, networking, and coordination
#[embassy_executor::task]
async fn core0_main_task(
    spawner: embassy_executor::Spawner,
    adc: Adc<'static, embassy_rp::adc::Async>,
    temp_sensor: Channel<'static>,
    pwr: Output<'static>,
    cs: Output<'static>,
    mut pio0: Pio<'static, PIO0>,
    dma_ch0: embassy_rp::Peri<'static, DMA_CH0>,
    pin_24: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_24>,
    pin_29: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_29>,
    watchdog: Watchdog,
) -> ! {
    info!("spawn watchdog_task");
    spawner.spawn(health::watchdog_task(watchdog)).ok();

    info!("spawn temp_task");
    spawner.spawn(temp_task(adc, temp_sensor)).ok();

    info!("Load the CYW43 firmware bytes");
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    info!("define spi for CYW43");
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio0.irq0,
        cs,
        pin_24,
        pin_29,
        dma_ch0,
    );

    static CYW43_STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = CYW43_STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;

    info!("spawn cyw43_task");
    spawner.spawn(cyw43_task(runner)).ok();

    info!("initialize cyw43 controller");
    control.init(clm).await;

    // Enable SMPS PWM mode for lower voltage ripple (at cost of efficiency)
    control.gpio_set(1, true).await;

    info!("enable cyw43 power save mode");
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // Static IP from build env; /24 with gateway x.x.x.1
    let ip: Ipv4Address = env!("IP").parse().unwrap();
    let subnet_prefix: u8 = 24;
    let cidr = Ipv4Cidr::new(ip, subnet_prefix);

    let mut gateway_octets: [u8; 4] = cidr.network().address().octets();
    gateway_octets[3] = 1;
    let gateway: Ipv4Address = gateway_octets.into();

    let dns_server: Ipv4Address = "1.1.1.1".parse().unwrap();
    let net_config = NetConfig::ipv4_static(StaticConfigV4 {
        address: cidr,
        gateway: Some(gateway),
        dns_servers: heapless::Vec::from_slice(&[dns_server]).unwrap(),
    });

    // Sockets: web server (1) + NTP (1) + DNS (1) + spare
    static STACK_RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();

    info!("configure network: {:?}", net_config);
    let (stack, runner) = embassy_net::new(
        net_device,
        net_config,
        STACK_RESOURCES.init(StackResources::new()),
        0x0123_4567_89ab_cdef,
    );

    info!("spawn net_task");
    spawner.spawn(net_task(runner)).ok();

    join_wifi(&mut control, stack).await;

    info!("start NTP");
    ntp::init(&spawner, stack);

    info!("start the web server");
    let app = make_static!(AppRouter<WebApp>, WebApp.build_app());
    let web_config = make_static!(
        picoserve::Config<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            persistent_start_read_request: Some(Duration::from_secs(1)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );
    spawner.spawn(web_task(stack, app, web_config)).ok();

    // Blink the onboard LED as a heartbeat; rejoin WiFi if the link drops
    loop {
        Timer::after_secs(2).await;
        control.gpio_set(0, false).await;
        Timer::after_secs(2).await;

        if stack.is_link_up() {
            control.gpio_set(0, true).await;
        } else {
            warn!("net stack link down, reconnecting...");
            join_wifi(&mut control, stack).await;
        }
    }
}

/// Entry point - sets up multicore execution
#[cortex_m_rt::entry]
fn main() -> ! {
    init_boot_time();

    info!("air-quality: multicore starting");
    let p = embassy_rp::init(Default::default());

    // Why did we reset? (read before the watchdog is started)
    let mut watchdog = Watchdog::new(p.WATCHDOG);
    let reset_cause = health::read_reset_cause(&mut watchdog);
    if reset_cause.is_fault() {
        warn!("last reset: {} (fault)", reset_cause.as_str());
    } else {
        info!("last reset: {}", reset_cause.as_str());
    }

    // ========== Core 1 peripherals (two SEN55s, one per I2C bus) ==========
    let sensor0 = Sensor0Resources {
        i2c: p.I2C0,
        sda: p.PIN_4,
        scl: p.PIN_5,
    };
    let sensor1 = Sensor1Resources {
        i2c: p.I2C1,
        sda: p.PIN_6,
        scl: p.PIN_7,
    };

    // ========== Core 0 peripherals (WiFi, ADC) ==========
    let adc = Adc::new(p.ADC, Irqs, AdcConfig::default());
    let temp_sensor = Channel::new_temp_sensor(p.ADC_TEMP_SENSOR);

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let pio0 = Pio::new(p.PIO0, Irqs);
    let dma_ch0 = p.DMA_CH0;
    let pin_24 = p.PIN_24;
    let pin_29 = p.PIN_29;

    // ========== Spawn Core 1 (sensor tasks) ==========
    info!("spawning Core 1 for sensor tasks");
    spawn_core1(
        p.CORE1,
        unsafe { &mut *addr_of_mut!(CORE1_STACK) },
        move || {
            info!("Core 1: executor starting");
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                spawner.spawn(sensor0_task(sensor0)).unwrap();
                spawner.spawn(sensor1_task(sensor1)).unwrap();
            });
        },
    );

    // ========== Run Core 0 executor ==========
    info!("Core 0: executor starting");
    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        spawner
            .spawn(core0_main_task(
                spawner,
                adc,
                temp_sensor,
                pwr,
                cs,
                pio0,
                dma_ch0,
                pin_24,
                pin_29,
                watchdog,
            ))
            .unwrap();
    });
}
