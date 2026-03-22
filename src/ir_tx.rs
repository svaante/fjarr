use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use esp_hal::{
    gpio::AnyPin,
    peripherals::RMT,
    rmt::{PulseCode, Rmt, TxChannelAsync, TxChannelConfig, TxChannelCreatorAsync},
    time::RateExtU32,
};
use esp_println::println;
use heapless::Vec;

use crate::recording::PulsePairs;

const CLK_DIV: u8 = 80;
const CARRIER_HIGH: u16 = 1052;
const CARRIER_LOW: u16 = 1053;

static TX_CMD: Signal<CriticalSectionRawMutex, PulsePairs> = Signal::new();

pub fn send(pulses: PulsePairs) {
    TX_CMD.signal(pulses);
}

#[embassy_executor::task]
pub async fn ir_tx_task(gpio: AnyPin, rmt_peripheral: RMT) {
    let rmt = Rmt::new(rmt_peripheral, 80u32.MHz()).unwrap().into_async();

    let tx_cfg = TxChannelConfig {
        clk_divider: CLK_DIV,
        idle_output_level: false,
        idle_output: false,
        carrier_modulation: true,
        carrier_high: CARRIER_HIGH,
        carrier_low: CARRIER_LOW,
        carrier_level: true,
    };

    let mut channel = rmt.channel0.configure(gpio, tx_cfg).unwrap();

    loop {
        let pulses = TX_CMD.wait().await;
        println!("ir_tx: {} pulse pairs", pulses.len());

        let mut rmt_data: Vec<u32, 128> = Vec::new();
        for &(mark_us, space_us) in pulses.iter() {
            let m = mark_us.min(0x7FFF);
            let s = space_us.min(0x7FFF);
            rmt_data
                .push(<u32 as PulseCode>::new(true, m, false, s))
                .ok();
        }
        // Ensure that last pulse ends in zero (RMT requirement)
        rmt_data.push(<u32 as PulseCode>::empty()).ok();

        match channel.transmit(&rmt_data).await {
            Ok(_) => println!("ir_tx: sent"),
            Err(e) => println!("ir_tx: transmit error: {:?}", e),
        }
    }
}
