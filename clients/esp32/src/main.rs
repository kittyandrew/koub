#![no_std]
#![no_main]

// @NOTE: Required in no_std for the global allocator (esp-alloc) and heap-using
// dependencies (esp-radio, reqwless, embassy-net internals).
extern crate alloc;

#[unsafe(no_mangle)]
fn _esp_println_timestamp() -> u64 {
    embassy_time::Instant::now().as_millis()
}

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources, dns::DnsSocket, tcp::client::TcpClient, tcp::client::TcpClientState};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::{clock::CpuClock, gpio::Level, gpio::Output, gpio::OutputConfig, ram, rng::Rng, timer::timg::TimerGroup};
use esp_radio::Controller;
use esp_radio::wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState};
use reqwless::{client::HttpClient, client::TlsConfig, client::TlsVerify, request::RequestBuilder};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("OUBOT_WIFI_SSID");
const PASSWORD: &str = env!("OUBOT_WIFI_PASS");
const SERVER: &str = env!("OUBOT_SERVER");
const TOKEN: &str = env!("OUBOT_TOKEN");

const HEARTBEAT_SECS: u64 = 7;
const BACKOFF_BASE_SECS: u64 = 2;
const MAX_BACKOFF_SECS: u64 = 12;
const MAX_AUTH_FAILURES: u32 = 5;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

fn auth_header() -> heapless::String<64> {
    const { assert!("token ".len() + TOKEN.len() <= 64, "OUBOT_TOKEN too long for auth header buffer") };
    let mut s = heapless::String::new();
    s.push_str("token ").unwrap();
    s.push_str(TOKEN).unwrap();
    s
}

fn backoff_secs(failures: u32) -> u64 {
    (BACKOFF_BASE_SECS * 2u64.pow(failures.saturating_sub(1).min(3))).min(MAX_BACKOFF_SECS)
}

/// Wait for backoff duration. LED is expected to already be ON (LOW) from caller.
/// @NOTE: GPIO8 is active-low: LOW = on, HIGH = off.
async fn error_wait(failures: u32) {
    Timer::after(Duration::from_secs(backoff_secs(failures))).await;
}

async fn halt(led: &mut Output<'_>) -> ! {
    led.set_low();
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    const { assert!(!SSID.is_empty(), "OUBOT_WIFI_SSID must not be empty") };
    const { assert!(!PASSWORD.is_empty(), "OUBOT_WIFI_PASS must not be empty") };
    const { assert!(!SERVER.is_empty(), "OUBOT_SERVER must not be empty") };
    const { assert!(!TOKEN.is_empty(), "OUBOT_TOKEN must not be empty") };
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // @NOTE: GPIO8 LED is active-low on this board (LOW = on, HIGH = off).
    let mut led = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let (controller, interfaces) = esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

    let wifi_interface = interfaces.sta;
    let net_config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        // @NOTE: 5 socket slots — DHCP(1) + DNS(1) + TCP(1) + 2 spare.
        // Spare slots prevent silent failures during reconnect overlap.
        mk_static!(StackResources<5>, StackResources::<5>::new()),
        seed,
    );

    if let Err(e) = spawner.spawn(connection(controller)) {
        log::error!("Failed to spawn WiFi task: {:?}", e);
        halt(&mut led).await;
    }
    if let Err(e) = spawner.spawn(net_task(runner)) {
        log::error!("Failed to spawn network task: {:?}", e);
        halt(&mut led).await;
    }

    // Wait for WiFi + IP. LED ON during connect (error = loud).
    led.set_low(); // active-low: LOW = on
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    log::info!("Waiting for IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            log::info!("Got IP: {}", config.address);
            led.set_high(); // LED OFF — connected
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let auth = auth_header();
    let headers = [("Authorization", auth.as_str())];
    log::info!("Starting heartbeat to {}/api/v1/up", SERVER);

    // Persistent connection heartbeat loop.
    // Outer loop: (re)establishes the TCP+TLS connection.
    // Inner loop: sends heartbeats on the persistent connection.
    // @NOTE: WiFi reconnect is handled by the connection() task — it monitors
    // StaDisconnected events and auto-reconnects. Unlike the Pico W client,
    // no explicit is_link_up() check is needed here.
    let mut failures: u32 = 0;
    let mut auth_failures: u32 = 0;
    loop {
        let mut tls_rx = [0; 4096];
        let mut tls_tx = [0; 4096];
        let dns = DnsSocket::new(stack);
        let tcp_state = TcpClientState::<1, 4096, 4096>::new();
        let tcp = TcpClient::new(stack, &tcp_state);

        // @WARNING: No TLS certificate verification — encrypted but server identity is
        // not validated (MITM risk). embedded-tls doesn't support cert verification in
        // no_std environments. Same limitation on the Pico W client (TlsVerify::None).
        let tls_seed = (rng.random() as u64) << 32 | rng.random() as u64;
        let tls = TlsConfig::new(tls_seed, &mut tls_rx, &mut tls_tx, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);

        // @NOTE: resource() uses the URL path as a base for get()/post() calls.
        // We pass the server root here; the endpoint path goes in get().
        let resource = client.resource(SERVER).await;
        let mut resource = match resource {
            Ok(r) => {
                log::info!("Connection established");
                led.set_high(); // LED OFF — connected
                r
            }
            Err(e) => {
                log::error!("Connect failed: {:?}", e);
                failures += 1;
                led.set_low(); // LED ON — error
                error_wait(failures).await;
                continue; // LED stays ON through next iteration
            }
        };

        // Inner loop: send heartbeats on the persistent connection.
        loop {
            let mut buffer = [0u8; 512];
            // @NOTE: async block emulates a try-block (unstable) so we can use ? for error collection.
            let result = async {
                let req = resource.get("/api/v1/up");
                let req = req.headers(&headers);
                let resp = req.send(&mut buffer).await?;
                Ok::<u16, reqwless::Error>(resp.status.0)
            }
            .await;

            match result {
                Ok(200) => {
                    // Brief blink on success (100ms pulse). Active-low: LOW = on.
                    led.set_low();
                    Timer::after(Duration::from_millis(100)).await;
                    led.set_high();
                    if failures > 0 {
                        log::info!("up: ok (recovered after {} failures)", failures);
                    } else {
                        log::info!("up: ok");
                    }
                    failures = 0;
                    auth_failures = 0;
                    Timer::after(Duration::from_secs(HEARTBEAT_SECS)).await;
                }
                Ok(status) => {
                    if status == 401 {
                        auth_failures += 1;
                        log::error!(
                            "up: 401 unauthorized — token is invalid, re-flash with correct OUBOT_TOKEN ({}/{})", auth_failures,
                            MAX_AUTH_FAILURES
                        );
                        if auth_failures >= MAX_AUTH_FAILURES {
                            log::error!("up: {} consecutive 401s — halting. Re-flash with valid OUBOT_TOKEN.", MAX_AUTH_FAILURES);
                            halt(&mut led).await;
                        }
                    } else {
                        log::warn!("up: unexpected status {}", status);
                        auth_failures = 0;
                    }
                    failures += 1;
                    led.set_low(); // LED ON — error
                    error_wait(failures).await;
                    // Non-200 doesn't necessarily mean connection is dead, keep trying.
                }
                Err(e) => {
                    // Connection error — break to outer loop to reconnect.
                    log::error!("up: request failed: {:?}", e);
                    failures += 1;
                    auth_failures = 0; // Reset — consecutive 401 streak broken by network error.
                    led.set_low(); // LED ON — error
                    error_wait(failures).await;
                    break; // Reconnect — LED stays ON through outer loop
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    log::info!("WiFi task started");
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                log::warn!("WiFi disconnected, reconnecting...");
                Timer::after(Duration::from_millis(5000)).await;
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(ClientConfig::default().with_ssid(SSID.into()).with_password(PASSWORD.into()));
            if let Err(e) = controller.set_config(&client_config) {
                log::error!("WiFi set_config failed: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
                continue;
            }
            log::info!("Starting WiFi...");
            if let Err(e) = controller.start_async().await {
                log::error!("WiFi start failed: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
                continue;
            }
            log::info!("WiFi started");
        }
        log::info!("Connecting to '{}'...", SSID);
        match controller.connect_async().await {
            Ok(_) => log::info!("WiFi connected!"),
            Err(e) => {
                log::error!("WiFi connect failed: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}
