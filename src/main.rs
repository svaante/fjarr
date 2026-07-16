#![no_std]
#![no_main]

mod cmd;
mod heartbeat;
mod http;
mod ir_rx;
mod ir_tx;
mod mdns;
mod recording;
mod storage;
mod ws;

use embassy_executor::Spawner;
use embassy_net::{Config, Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::Pin,
    interrupt::{software::SoftwareInterruptControl, Priority},
    rng::Rng,
    rtc_cntl::{Rtc, RwdtStage, RwdtStageAction},
    time::ExtU64,
    timer::systimer::SystemTimer,
    timer::timg::TimerGroup,
};
use esp_hal_embassy::InterruptExecutor;
use esp_println::println;
use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiEvent, WifiStaDevice,
};
use esp_wifi::{wifi::WifiDevice, EspWifiController};
use static_cell::StaticCell;

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

static WIFI_INIT: StaticCell<EspWifiController<'static>> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
static STACK: StaticCell<Stack<'static>> = StaticCell::new();
// NOTE: High-priority executor keeps IR edge timestamps accurate
static IR_RX_EXECUTOR: StaticCell<InterruptExecutor<0>> = StaticCell::new();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    esp_alloc::heap_allocator!(96 * 1024);

    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));
    esp_hal_embassy::init(TimerGroup::new(p.TIMG0).timer0);

    let mut rwdt = Rtc::new(p.LPWR).rwdt;
    rwdt.enable();
    rwdt.set_timeout(RwdtStage::Stage0, 8u64.secs());
    rwdt.set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);

    {
        let saved = storage::load();
        let mut sigs = recording::SIGNALS.lock().await;
        for s in saved {
            sigs.push(s).ok();
        }
    }

    let systimer = SystemTimer::new(p.SYSTIMER);
    let wifi_init: &'static _ =
        WIFI_INIT.init(esp_wifi::init(systimer.alarm0, Rng::new(p.RNG), p.RADIO_CLK).unwrap());

    let (wifi_device, wifi_controller) =
        esp_wifi::wifi::new_with_mode(wifi_init, p.WIFI, WifiStaDevice).unwrap();

    let (stack, runner) = embassy_net::new(
        wifi_device,
        Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        0xdead_beef_cafe_babe,
    );
    let stack: &'static Stack<'static> = STACK.init(stack);

    let sw_ints = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    let ir_rx_executor = IR_RX_EXECUTOR.init(InterruptExecutor::new(sw_ints.software_interrupt0));
    let ir_rx_spawner = ir_rx_executor.start(Priority::Priority2);
    ir_rx_spawner
        .spawn(ir_rx::ir_rx_task(p.GPIO0.degrade()))
        .unwrap();

    spawner
        .spawn(ir_tx::ir_tx_task(p.GPIO1.degrade(), p.RMT))
        .unwrap();

    spawner.spawn(connection(wifi_controller)).unwrap();
    spawner.spawn(net_task(runner)).unwrap();
    spawner.spawn(mdns::mdns_task(stack)).unwrap();

    spawner.spawn(http::https_reject_task(stack)).unwrap();
    for i in 0..http::HTTP_TASK_COUNT {
        spawner.spawn(http::http_task(stack, i)).unwrap();
    }
    spawner.spawn(ws::ws_task(stack)).unwrap();
    spawner
        .spawn(heartbeat::watchdog(p.GPIO7.degrade(), p.LEDC, stack, rwdt))
        .unwrap();

    println!("main: waiting for wifi...");
    loop {
        if let Some(cfg) = stack.config_v4() {
            println!("main: connected, ip: {}", cfg.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("main: ir decoder ready");
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("wifi: starting...");
            controller.start_async().await.unwrap();
        }

        controller
            .set_power_saving(esp_wifi::config::PowerSaveMode::Maximum)
            .unwrap();

        println!("wifi: connecting to {}...", SSID);
        match controller.connect_async().await {
            Ok(_) => println!("wifi: connected"),
            Err(e) => {
                println!("wifi: connect failed: {:?}", e);
                Timer::after(Duration::from_secs(5)).await;
                continue;
            }
        }

        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        println!("wifi: disconnected, retrying...");
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await
}
