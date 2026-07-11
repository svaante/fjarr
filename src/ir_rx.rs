use embassy_time::{Duration, Instant, with_timeout};
use esp_hal::gpio::{AnyPin, Input, Pull};
use esp_println::println;

use crate::recording::{PulsePairs, RECORDING, RecordingState};

const SILENCE_MS: u64 = 25;
const MIN_PULSE_US: u32 = 50;

#[embassy_executor::task]
pub async fn ir_rx_task(gpio: AnyPin) {
    let mut pin = Input::new(gpio, Pull::Up);

    loop {
        match with_timeout(Duration::from_millis(SILENCE_MS), pin.wait_for_any_edge()).await {
            Ok(()) => {
                let mut rec = RECORDING.lock().await;
                if let RecordingState::Capturing { timestamps } = &mut *rec {
                    let is_idle = pin.is_high();
                    // Ensure timestamps alternate [active, idle, active, ...]
                    if (timestamps.len() & 1 == 0) == is_idle {
                        continue;
                    }
                    let now = Instant::now().as_micros() as u32;
                    // Low pass for noise
                    if timestamps
                        .last()
                        .is_some_and(|&t| now.wrapping_sub(t) < MIN_PULSE_US)
                    {
                        continue;
                    }
                    timestamps.push(now).ok();
                }
            }
            Err(_) => {
                // Timeout means end of signal
                let mut rec = RECORDING.lock().await;
                if let RecordingState::Capturing { timestamps } = &*rec {
                    let mut deltas = timestamps
                        .windows(2)
                        .map(|w| u16::try_from(w[1].wrapping_sub(w[0])).unwrap_or(u16::MAX));

                    let pulses: PulsePairs =
                        core::iter::from_fn(|| Some((deltas.next()?, deltas.next().unwrap_or(0))))
                            .collect();

                    if pulses.is_empty() {
                        continue;
                    }

                    println!("ir_rx: captured {} pulse pairs", pulses.len());
                    *rec = RecordingState::Done { pulses };
                    crate::recording::STATE_CHANGED.signal(());
                }
            }
        }
    }
}
