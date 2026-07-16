use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use esp_hal::{
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    rtc_cntl::Rwdt,
    time::RateExtU32,
};

const PEAK_DUTY: u8 = 2;
const FADE_MS: u16 = 50;

#[embassy_executor::task]
pub async fn watchdog(
    gpio: esp_hal::gpio::AnyPin,
    ledc: esp_hal::peripherals::LEDC,
    stack: &'static Stack<'static>,
    mut rwdt: Rwdt,
) {
    let mut ledc = Ledc::new(ledc);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut lstimer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: 1u32.kHz(),
        })
        .unwrap();
    let mut channel = ledc.channel(channel::Number::Channel0, gpio);
    channel
        .configure(channel::config::Config {
            timer: &lstimer,
            duty_pct: 0,
            pin_config: channel::config::PinConfig::PushPull,
        })
        .unwrap();

    loop {
        rwdt.feed();
        channel.start_duty_fade(0, PEAK_DUTY, FADE_MS).unwrap();
        Timer::after(Duration::from_millis(FADE_MS as u64)).await;
        channel.start_duty_fade(PEAK_DUTY, 0, FADE_MS).unwrap();
        Timer::after(Duration::from_millis(if stack.is_link_up() {
            4900
        } else {
            500
        }))
        .await;
    }
}
