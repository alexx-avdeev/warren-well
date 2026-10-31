#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

mod app;

use bt_hci::controller::ExternalController;
use embassy_executor::Spawner;
use embassy_net::StackResources;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use esp_radio::wifi::{ControllerConfig, Interface, WifiController};
use log::info;
use tank_core::FIRMWARE_VERSION;
use tank_core::payload::device_id;

use app::state::{BLE_WINDOW, BOOT_ID, CONFIG, DEVICE_ID};

extern crate alloc;

// Default app descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// Sockets: 2 web listeners + DHCP + DNS + 1 spare for the future HTTP client.
const STACK_SOCKETS: usize = app::web::WEB_TASK_POOL_SIZE + 3;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.4.0
    // generator parameters: -o esp32c6 -o unstable-hal -o alloc -o wifi -o ble-trouble -o embassy -o log -o esp-backtrace -o claude

    esp_println::logger::init_logger_from_env();
    info!("rabbit tank monitor fw {FIRMWARE_VERSION}");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
    // COEX needs more RAM - so we've added some more
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // --- persistent config -------------------------------------------------
    let mut store = app::storage::ConfigStore::new(peripherals.FLASH);
    match store.load().await {
        Some(cfg) => *CONFIG.lock().await = cfg,
        None => apply_build_defaults(&mut *CONFIG.lock().await),
    }

    // --- radios -------------------------------------------------------------
    let rng = Rng::new();
    BOOT_ID.store(rng.random().max(1), core::sync::atomic::Ordering::Relaxed);
    let transport = BleConnector::new(peripherals.BT, Default::default()).expect("BLE connector");
    let ble_controller = ExternalController::<_, 1>::new(transport);

    #[cfg(not(feature = "ble-only"))]
    let mac = {
        let wifi_controller = WifiController::new(peripherals.WIFI, ControllerConfig::default())
            .expect("Failed to initialize Wi-Fi controller");
        let wifi_interface = Interface::station();
        let mac = wifi_interface.mac_address();

        let seed = ((rng.random() as u64) << 32) | rng.random() as u64;
        let (stack, runner) = embassy_net::new(
            wifi_interface,
            embassy_net::Config::dhcpv4(Default::default()),
            mk_static!(StackResources<STACK_SOCKETS>, StackResources::new()),
            seed,
        );
        spawner.spawn(app::wifi::net_task(runner).unwrap());
        spawner.spawn(app::wifi::connection_task(wifi_controller, stack).unwrap());
        for id in 0..app::web::WEB_TASK_POOL_SIZE {
            spawner.spawn(app::web::web_task(id, stack).unwrap());
        }
        spawner.spawn(app::reporter::reporter_task(app::reporter::HttpReporter::new(stack)).unwrap());
        mac
    };
    #[cfg(feature = "ble-only")]
    let mac: [u8; 6] = {
        info!("ble-only diagnostic build: WiFi and web disabled");
        [0xf0, 0xf5, 0xbd, 0x0b, 0xec, 0xe4]
    };
    *DEVICE_ID.lock().await = device_id(&mac);
    info!("device id {}", device_id(&mac));

    // --- tasks --------------------------------------------------------------
    spawner.spawn(app::ble::ble_task(ble_controller, mac).unwrap());
    spawner.spawn(button_task(Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    )).unwrap());

    #[cfg(feature = "sim-sensor")]
    spawner.spawn(app::level_task::level_task(rng.random()).unwrap());
    #[cfg(not(feature = "sim-sensor"))]
    {
        use esp_hal::uart::{Config as UartConfig, UartRx};
        let rx = UartRx::new(peripherals.UART1, UartConfig::default().with_baudrate(9600))
            .expect("uart1")
            .with_rx(peripherals.GPIO4)
            .into_async();
        spawner.spawn(app::level_task::level_task(rx).unwrap());
    }

    // BLE is discoverable for a few minutes after every boot.
    BLE_WINDOW.signal(());

    // Control loop owns the flash store and runs forever.
    app::control::run(store).await
}

/// Development convenience: `WIFI_SSID`, `WIFI_PASSWORD`, `API_URL` and `API_TOKEN` set in
/// the build environment seed the config when flash is empty, so a freshly
/// erased board joins the network without BLE provisioning.
fn apply_build_defaults(cfg: &mut tank_core::config::DeviceConfig) {
    if let Some(ssid) = option_env!("WIFI_SSID") {
        let _ = cfg.wifi_ssid.push_str(ssid);
        info!("using build-time WiFi defaults for '{ssid}'");
    }
    if let Some(pw) = option_env!("WIFI_PASSWORD") {
        let _ = cfg.wifi_password.push_str(pw);
    }
    if let Some(url) = option_env!("API_URL") {
        let _ = cfg.api_url.push_str(url);
    }
    if let Some(token) = option_env!("API_TOKEN") {
        let _ = cfg.api_token.push_str(token);
    }
}

/// BOOT button (GPIO9, active low): press to open the BLE provisioning window.
#[embassy_executor::task]
async fn button_task(mut button: Input<'static>) {
    loop {
        button.wait_for_falling_edge().await;
        embassy_time::Timer::after_millis(50).await; // debounce
        if button.is_low() {
            info!("button: opening BLE window");
            BLE_WINDOW.signal(());
            button.wait_for_rising_edge().await;
        }
    }
}
