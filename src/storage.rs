use embedded_storage::{ReadStorage, Storage};
use esp_println::println;
use esp_storage::FlashStorage;
use heapless::Vec;

use crate::recording::{NameStr, PulsePairs, RecordedSignal};

const FLASH_OFFSET: u32 = 0x3F0000;

fn encoded_len(sig: &RecordedSignal) -> usize {
    1 + sig.name.len() + 1 + sig.device.len() + 1 + sig.pulses.len() * 4
}

fn encode_signal(sig: &RecordedSignal, buf: &mut [u8]) -> usize {
    let mut pos = 0;

    // Encoding strings as u8 length + u8 [0..length]
    for field in [sig.name.as_bytes(), sig.device.as_bytes()] {
        buf[pos] = field.len() as u8;
        pos += 1;
        buf[pos..pos + field.len()].copy_from_slice(field);
        pos += field.len();
    }

    // Encoding strings as u8 nof signals + signals
    buf[pos] = sig.pulses.len() as u8;
    pos += 1;
    for &(mark, space) in sig.pulses.iter() {
        buf[pos..pos + 2].copy_from_slice(&mark.to_le_bytes());
        pos += 2;
        buf[pos..pos + 2].copy_from_slice(&space.to_le_bytes());
        pos += 2;
    }
    pos
}

fn encode(signals: &Vec<RecordedSignal, 16>, buf: &mut [u8]) -> usize {
    let mut pos = 1; // byte 0 reserved for count
    let mut count = 0u8;
    for sig in signals.iter() {
        if pos + encoded_len(sig) > buf.len() {
            break;
        }
        pos += encode_signal(sig, &mut buf[pos..]);
        count += 1;
    }
    buf[0] = count;
    pos
}

fn decode_signal(buf: &[u8]) -> Option<(RecordedSignal, usize)> {
    let mut pos = 0;

    let nl = *buf.get(pos)? as usize;
    pos += 1;
    let mut name = NameStr::new();
    name.push_str(core::str::from_utf8(buf.get(pos..pos + nl)?).unwrap_or(""))
        .ok();
    pos += nl;

    let dl = *buf.get(pos)? as usize;
    pos += 1;
    let mut device = NameStr::new();
    device
        .push_str(core::str::from_utf8(buf.get(pos..pos + dl)?).unwrap_or(""))
        .ok();
    pos += dl;

    let pc = *buf.get(pos)? as usize;
    pos += 1;
    let mut pulses = PulsePairs::new();
    for _ in 0..pc {
        let pair = buf.get(pos..pos + 4)?;
        pulses
            .push((
                u16::from_le_bytes(pair[..2].try_into().unwrap()),
                u16::from_le_bytes(pair[2..].try_into().unwrap()),
            ))
            .ok();
        pos += 4;
    }
    Some((
        RecordedSignal {
            name,
            device,
            pulses,
        },
        pos,
    ))
}

fn decode(buf: &[u8]) -> Vec<RecordedSignal, 16> {
    let mut signals = Vec::new();
    if buf.is_empty() {
        return signals;
    }
    let count = buf[0] as usize;
    let mut pos = 1;
    for _ in 0..count {
        match decode_signal(&buf[pos..]) {
            Some((sig, n)) => {
                pos += n;
                signals.push(sig).ok();
            }
            None => break,
        }
    }
    signals
}

pub fn load() -> Vec<RecordedSignal, 16> {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; 4096];
    if flash.read(FLASH_OFFSET, &mut buf).is_err() {
        println!("storage: read error");
        return Vec::new();
    }
    let signals = decode(&buf);
    println!("storage: loaded {} signals", signals.len());
    signals
}

pub fn save(signals: &Vec<RecordedSignal, 16>) {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; 4096];
    let len = encode(signals, &mut buf);
    let write_len = (len + 3) & !3;
    flash.write(FLASH_OFFSET, &buf[..write_len]).ok();
    println!("storage: saved {} signals", signals.len());
}
