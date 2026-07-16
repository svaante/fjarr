use embassy_futures::select::{select, Either};
use embassy_time::{Instant, Timer};
use esp_hal::gpio::{AnyPin, Input, Pull};
use esp_println::println;

use crate::recording::{PulsePairs, RecordingState, IR_RX_NOTIFY, RECORDING, WS_NOTIFY};

const SILENCE_US: u64 = 25_000;
const MIN_PULSE_US: u32 = 50;

#[embassy_executor::task]
pub async fn ir_rx_task(gpio: AnyPin) {
    let mut pin = Input::new(gpio, Pull::Up);

    loop {
        // Wait until capture is requested
        loop {
            IR_RX_NOTIFY.wait().await;
            if matches!(&*RECORDING.lock().await, RecordingState::Capturing { .. }) {
                break;
            }
        }

        println!("ir_rx: capture started");

        loop {
            match select(pin.wait_for_any_edge(), Timer::after_micros(SILENCE_US)).await {
                Either::First(_) => {
                    let now = Instant::now().as_micros() as u32;
                    let mut rec = RECORDING.lock().await;
                    let RecordingState::Capturing { timestamps } = &mut *rec else {
                        break;
                    };

                    // Lowpass
                    if let Some(&last) = timestamps.last() {
                        if now.wrapping_sub(last) < MIN_PULSE_US {
                            continue;
                        }
                    }

                    // Check sequence of SPACEs MARKs
                    let is_idle = pin.is_high();
                    if (timestamps.len() & 1 == 0) == is_idle {
                        continue;
                    }
                    timestamps.push(now).ok();
                    if timestamps.len() == timestamps.capacity() {
                        *rec = RecordingState::Idle;
                        WS_NOTIFY.signal(());
                        break;
                    }
                }
                // EOF
                Either::Second(_) => {
                    let mut rec = RECORDING.lock().await;
                    if let RecordingState::Capturing { timestamps } = &mut *rec {
                        let mut deltas = timestamps
                            .windows(2)
                            .map(|w| u16::try_from(w[1].wrapping_sub(w[0])).unwrap_or(u16::MAX));
                        let pulses: PulsePairs = core::iter::from_fn(|| {
                            Some((deltas.next()?, deltas.next().unwrap_or(0)))
                        })
                        .collect();
                        if pulses.is_empty() {
                            continue;
                        }
                        println!("ir_rx: captured {} pulse pairs", pulses.len());
                        *rec = RecordingState::Done { pulses };
                        WS_NOTIFY.signal(());
                    }
                    break;
                }
            }
        }

        println!("ir_rx: capture stopped");
    }
}
