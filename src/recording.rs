use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use heapless::{String, Vec};

pub type NameStr = String<64>;
pub type PulsePairs = Vec<(u16, u16), 128>;
pub type Timestamps = Vec<u32, 256>;

pub struct RecordedSignal {
    pub name: NameStr,
    pub device: NameStr,
    pub pulses: PulsePairs,
}

pub enum RecordingState {
    Idle,
    Capturing { timestamps: Timestamps },
    Done { pulses: PulsePairs },
}

pub static RECORDING: Mutex<CriticalSectionRawMutex, RecordingState> =
    Mutex::new(RecordingState::Idle);

pub static SIGNALS: Mutex<CriticalSectionRawMutex, Vec<RecordedSignal, 16>> =
    Mutex::new(Vec::new());

pub static STATE_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
