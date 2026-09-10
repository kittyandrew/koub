#![no_std]
#![no_main]

use cyw43::JoinOptions;
use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::{StackResources, dns::DnsSocket, tcp::client::TcpClient, tcp::client::TcpClientState};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, clocks::RoscRng, dma, gpio::Level, gpio::Output, peripherals::DMA_CH0, peripherals::PIO0};
use embassy_time::{Duration, Timer};
use reqwless::{client::HttpClient, client::TlsConfig, client::TlsVerify, request::RequestBuilder};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

const SSID: &str = env!("OUBOT_WIFI_SSID");
const PASSWORD: &str = env!("OUBOT_WIFI_PASS");
const SERVER: &str = env!("OUBOT_SERVER");
const TOKEN: &str = env!("OUBOT_TOKEN");

const HEARTBEAT_SECS: u64 = 7;
const BACKOFF_BASE_SECS: u64 = 2;
const MAX_BACKOFF_SECS: u64 = 12;
const MAX_AUTH_FAILURES: u32 = 5;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
});

fn auth_header() -> heapless::String<64> {
    // @NOTE: Use core::assert! because defmt overrides assert! and doesn't work in const blocks.
    const { core::assert!("token ".len() + TOKEN.len() <= 64, "OUBOT_TOKEN too long for auth header buffer") };
    let mut s = heapless::String::new();
    s.push_str("token ").unwrap();
    s.push_str(TOKEN).unwrap();
    s
}

fn backoff_secs(failures: u32) -> u64 {
    (BACKOFF_BASE_SECS * 2u64.pow(failures.saturating_sub(1).min(3))).min(MAX_BACKOFF_SECS)
}

/// LED contract: OFF = normal/idle, brief ON blink = success, solid ON = error/reconnect.
/// LED on = cyw43 GPIO0 high; LED off = GPIO0 low. The Pico W LED is active-high
/// but controlled through the WiFi chip, not a regular RP2040 GPIO.
async fn success_blink(control: &mut cyw43::Control<'_>) {
    control.gpio_set(0, true).await;
    Timer::after(Duration::from_millis(100)).await;
    control.gpio_set(0, false).await;
}

/// Wait for backoff duration. LED is expected to already be ON from caller.
async fn error_wait(failures: u32) {
    Timer::after(Duration::from_secs(backoff_secs(failures))).await;
}

async fn halt(control: &mut cyw43::Control<'_>) -> ! {
    control.gpio_set(0, true).await;
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

async fn wifi_connect(control: &mut cyw43::Control<'_>, stack: embassy_net::Stack<'_>) {
    loop {
        info!("Connecting to '{}'...", SSID);
        match control.join(SSID, JoinOptions::new(PASSWORD.as_bytes())).await {
            Ok(_) => {
                info!("WiFi connected!");
                break;
            }
            Err(e) => {
                error!("WiFi join failed: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
    info!("Waiting for IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {:?}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    const { core::assert!(!SSID.is_empty(), "OUBOT_WIFI_SSID must not be empty") };
    const { core::assert!(!PASSWORD.is_empty(), "OUBOT_WIFI_PASS must not be empty") };
    const { core::assert!(!SERVER.is_empty(), "OUBOT_SERVER must not be empty") };
    const { core::assert!(!TOKEN.is_empty(), "OUBOT_TOKEN must not be empty") };

    let p = embassy_rp::init(Default::default());

    // @NOTE: Firmware files from the embassy-rs/embassy repository (Cypress/Infineon
    // Permissive Binary License 1.0). The CYW43439 has no flash — firmware loads over
    // SPI on every boot. ~231KB firmware + ~742B NVRAM + ~984B CLM.
    let fw = cyw43::aligned_bytes!("../firmware/43439A0.bin");
    let nvram = cyw43::aligned_bytes!("../firmware/nvram_rp2040.bin");
    let clm = include_bytes!("../firmware/43439A0_clm.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio0 = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0,
        cyw43_pio::RM2_CLOCK_DIVIDER,
        pio0.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;

    match cyw43_task(runner) {
        Ok(token) => spawner.spawn(token),
        Err(_) => {
            error!("Failed to spawn cyw43 task");
            // @NOTE: Can't call halt() here — control requires cyw43 runner to be running.
            loop {
                Timer::after(Duration::from_secs(3600)).await;
            }
        }
    }

    control.init(clm).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    let mut rng = RoscRng;
    let seed = rng.next_u64();

    let net_config = embassy_net::Config::dhcpv4(Default::default());

    // @NOTE: 5 socket slots — DHCP(1) + DNS(1) + TCP(1) + 2 spare.
    // Spare slots prevent silent failures during reconnect overlap.
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(net_device, net_config, RESOURCES.init(StackResources::<5>::new()), seed);

    match net_task(net_runner) {
        Ok(token) => spawner.spawn(token),
        Err(_) => {
            error!("Failed to spawn network task");
            halt(&mut control).await;
        }
    }

    // Connect WiFi + wait for IP. LED ON during connect (error = loud).
    control.gpio_set(0, true).await;
    wifi_connect(&mut control, stack).await;
    control.gpio_set(0, false).await;

    let auth = auth_header();
    let headers = [("Authorization", auth.as_str())];
    info!("Starting heartbeat to {}/api/v1/up", SERVER);

    // Persistent connection heartbeat loop.
    // Outer loop: (re)establishes the TCP+TLS connection.
    // Inner loop: sends heartbeats on the persistent connection.
    let mut failures: u32 = 0;
    let mut auth_failures: u32 = 0;
    loop {
        // @NOTE: cyw43 detects WiFi drops (LINK/DEAUTH events) and sets link_state
        // to Down, but does NOT auto-reconnect. Re-join if link dropped.
        if !stack.is_link_up() {
            warn!("WiFi link down, reconnecting...");
            control.gpio_set(0, true).await; // LED ON during reconnect
            wifi_connect(&mut control, stack).await;
            control.gpio_set(0, false).await;
        }
        let mut tls_rx = [0; 4096];
        let mut tls_tx = [0; 4096];
        let dns = DnsSocket::new(stack);
        let tcp_state = TcpClientState::<1, 4096, 4096>::new();
        let tcp = TcpClient::new(stack, &tcp_state);

        // @WARNING: No TLS certificate verification — encrypted but server identity is
        // not validated (MITM risk). Same limitation as ESP32 client and the old
        // MicroPython client (CERT_NONE). See docs/specs/esp32-security-metrics.md.
        let tls_seed = rng.next_u64();
        let tls = TlsConfig::new(tls_seed, &mut tls_rx, &mut tls_tx, TlsVerify::None);
        let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);

        let resource = client.resource(SERVER).await;
        let mut resource = match resource {
            Ok(r) => {
                info!("Connection established");
                control.gpio_set(0, false).await; // LED OFF — connected
                r
            }
            Err(e) => {
                error!("Connect failed: {:?}", e);
                failures += 1;
                control.gpio_set(0, true).await;
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
                    success_blink(&mut control).await;
                    if failures > 0 {
                        info!("up: ok (recovered after {} failures)", failures);
                    } else {
                        info!("up: ok");
                    }
                    failures = 0;
                    auth_failures = 0;
                    Timer::after(Duration::from_secs(HEARTBEAT_SECS)).await;
                }
                Ok(status) => {
                    if status == 401 {
                        auth_failures += 1;
                        error!(
                            "up: 401 unauthorized — token is invalid, re-flash with correct OUBOT_TOKEN ({}/{})",
                            auth_failures, MAX_AUTH_FAILURES
                        );
                        if auth_failures >= MAX_AUTH_FAILURES {
                            error!("up: {} consecutive 401s — halting. Re-flash with valid OUBOT_TOKEN.", MAX_AUTH_FAILURES);
                            halt(&mut control).await;
                        }
                    } else {
                        warn!("up: unexpected status {}", status);
                        auth_failures = 0;
                    }
                    failures += 1;
                    control.gpio_set(0, true).await;
                    error_wait(failures).await;
                    // Non-200 doesn't necessarily mean connection is dead, keep trying.
                }
                Err(e) => {
                    error!("up: request failed: {:?}", e);
                    failures += 1;
                    auth_failures = 0; // Reset — consecutive 401 streak broken by network error.
                    control.gpio_set(0, true).await;
                    error_wait(failures).await;
                    break; // Reconnect — LED stays ON through outer loop
                }
            }
        }
    }
}

// @NOTE: The cyw43 runner must run continuously — it handles WiFi chip communication
// (firmware commands, event processing, TX/RX). Stopping it kills WiFi and LED control.
#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}
