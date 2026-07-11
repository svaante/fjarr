use core::fmt::Write as FmtWrite;
use heapless::{String, Vec};

use crate::ir_tx;
use crate::recording::{NameStr, PulsePairs, RecordedSignal, RecordingState, Timestamps, RECORDING, SIGNALS};

async fn list_json(out: &mut String<1200>) {
    let sigs = SIGNALS.lock().await;
    out.push_str(r#"{"type":"list","devices":["#).ok();
    let mut seen: Vec<&str, 16> = Vec::new();
    for sig in sigs.iter() {
        let d = sig.device.as_str();
        if !seen.iter().any(|&s| s == d) {
            seen.push(d).ok();
        }
    }
    let mut first_dev = true;
    for dev in seen.iter() {
        if !first_dev {
            out.push(',').ok();
        }
        first_dev = false;
        write!(out, r#"{{"name":"{}","signals":["#, dev).ok();
        let mut first_sig = true;
        for sig in sigs.iter().filter(|s| s.device.as_str() == *dev) {
            if !first_sig {
                out.push(',').ok();
            }
            first_sig = false;
            write!(
                out,
                r#"{{"name":"{}","pulses":{}}}"#,
                sig.name.as_str(),
                sig.pulses.len()
            )
            .ok();
        }
        out.push_str("]}").ok();
    }
    out.push_str("]}").ok();
}

pub async fn dispatch(typ: &str, name: Option<&str>, device: Option<&str>, out: &mut String<1200>) {
    match typ {
        "record" => {
            let mut rec = RECORDING.lock().await;
            if !matches!(&*rec, RecordingState::Capturing { .. }) {
                *rec = RecordingState::Capturing { timestamps: Timestamps::new() };
                crate::recording::STATE_CHANGED.signal(());
            }
        }
        "discard" => {
            *RECORDING.lock().await = RecordingState::Idle;
            crate::recording::STATE_CHANGED.signal(());
        }
        "save" => {
            let Some(name_str) = name else { return };
            let mut nm: NameStr = NameStr::new();
            nm.push_str(name_str).ok();
            let mut dv: NameStr = NameStr::new();
            dv.push_str(device.unwrap_or("")).ok();
            let pulses = {
                let rec = RECORDING.lock().await;
                if let RecordingState::Done { pulses } = &*rec {
                    Some(pulses.clone())
                } else {
                    None
                }
            };
            if let Some(pulses) = pulses {
                *RECORDING.lock().await = RecordingState::Idle;
                crate::recording::STATE_CHANGED.signal(());
                {
                    let mut sigs = SIGNALS.lock().await;
                    sigs.push(RecordedSignal {
                        name: nm,
                        device: dv,
                        pulses,
                    })
                    .ok();
                    crate::storage::save(&sigs);
                }
                list_json(out).await;
            }
        }
        "test" => {
            let rec = RECORDING.lock().await;
            if let RecordingState::Done { pulses } = &*rec {
                ir_tx::send(pulses.clone());
            }
        }
        "list" => {
            list_json(out).await;
        }
        "tx" => {
            let Some(name_str) = name else { return };
            let dev_str = device.unwrap_or("");
            let pulses: Option<PulsePairs> = {
                let sigs = SIGNALS.lock().await;
                sigs.iter()
                    .find(|s| s.name.as_str() == name_str && s.device.as_str() == dev_str)
                    .map(|s| s.pulses.clone())
            };
            if let Some(p) = pulses {
                ir_tx::send(p);
            }
        }
        "delete" => {
            let Some(name_str) = name else { return };
            let dev_str = device.unwrap_or("");
            let mut sigs = SIGNALS.lock().await;
            if let Some(pos) = sigs
                .iter()
                .position(|s| s.name.as_str() == name_str && s.device.as_str() == dev_str)
            {
                sigs.swap_remove(pos);
            }
            crate::storage::save(&sigs);
            drop(sigs);
            list_json(out).await;
        }
        _ => {}
    }
}
