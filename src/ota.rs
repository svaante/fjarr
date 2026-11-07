// Firmware update over http: `POST /ota` on OTA_PORT with body = app image ++ ed25519 signature.
// The signature covers sha256(app image) and is checked against keys/ota.pub before the
// bootloader is pointed at the new slot. Layout must match partitions.csv.

use core::fmt::Write as FmtWrite;

use embassy_futures::yield_now;
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_println::println;
use esp_storage::FlashStorage;
use heapless::String;
use sha2::{Digest, Sha256};
use static_cell::StaticCell;

use crate::http::send_response;

pub const OTA_PORT: u16 = 3232;

const OTADATA: u32 = 0xD000;
const SLOTS: [u32; 2] = [0x10000, 0x200000];
const SLOT_SIZE: u32 = 0x1F0000;
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;
const SIG_LEN: usize = 64;

const IMAGE_MAGIC: u8 = 0xE9;
const CHIP_ID_ESP32C3: u16 = 5;

const PUBLIC_KEY: &[u8; 32] = include_bytes!("../keys/ota.pub");

// NOTE: ESP32-C3 flash MMU; one table shared by the instruction and data bus windows,
// one entry per 64 KiB page
const MMU_TABLE: *const u32 = 0x600C_5000 as *const u32;
const MMU_INVALID: u32 = 1 << 8;
const MMU_VADDR_MASK: usize = 0x7F_FFFF;
const DROM_BASE: usize = 0x3C00_0000;

#[repr(C, align(4))]
struct Aligned<const N: usize>([u8; N]);

static OTA_RX: StaticCell<[u8; 8192]> = StaticCell::new();
static OTA_TX: StaticCell<[u8; 512]> = StaticCell::new();
static HEAD_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
static SECTOR_BUF: StaticCell<Aligned<{ SECTOR as usize }>> = StaticCell::new();

fn crc32(data: &[u8]) -> u32 {
    // Matches esp_rom_crc32_le(UINT32_MAX, ..) used by the bootloader for otadata
    let mut crc = 0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// Resolve the running slot from the MMU instead of otadata; otadata may point at a slot
// the bootloader rejected and fell back from. PUBLIC_KEY sits in flash rodata, so translate
// its address and confirm the key bytes really are at the resulting flash offset; any
// mismatch returns None, which disables OTA rather than risk the running slot.
fn running_slot() -> Option<usize> {
    let vaddr = PUBLIC_KEY.as_ptr() as usize;
    if !(DROM_BASE..DROM_BASE + MMU_VADDR_MASK + 1).contains(&vaddr) {
        return None;
    }
    let entry = unsafe {
        MMU_TABLE
            .add((vaddr & MMU_VADDR_MASK) >> 16)
            .read_volatile()
    };
    if entry & MMU_INVALID != 0 {
        return None;
    }
    let paddr = ((entry & 0xFF) << 16) | (vaddr as u32 & 0xFFFF);

    let mut buf = Aligned([0u8; 36]);
    FlashStorage::new().read(paddr & !3, &mut buf.0).ok()?;
    let skew = (paddr & 3) as usize;
    if buf.0[skew..skew + 32] != PUBLIC_KEY[..] {
        return None;
    }

    SLOTS
        .iter()
        .position(|&base| (base..base + SLOT_SIZE).contains(&paddr))
}

fn read_seq(flash: &mut FlashStorage, sector: u32) -> Option<u32> {
    let mut e = Aligned([0u8; 32]);
    flash.read(OTADATA + sector * SECTOR, &mut e.0).ok()?;
    let seq = u32::from_le_bytes(e.0[0..4].try_into().unwrap());
    let crc = u32::from_le_bytes(e.0[28..32].try_into().unwrap());
    (seq != u32::MAX && crc == crc32(&e.0[0..4])).then_some(seq)
}

fn set_boot_slot(flash: &mut FlashStorage, slot: usize) -> Result<(), &'static str> {
    let seqs = [read_seq(flash, 0), read_seq(flash, 1)];
    let active = (0..2)
        .filter(|&i| seqs[i].is_some())
        .max_by_key(|&i| seqs[i]);
    let mut seq = active.and_then(|i| seqs[i]).unwrap_or(0) + 1;
    while (seq - 1) as usize % SLOTS.len() != slot {
        seq += 1;
    }

    // Write the sector not holding the current entry so a torn write keeps the old one
    let sector = active.map_or(0, |i| 1 - i as u32);
    let mut e = Aligned([0xFFu8; 32]);
    e.0[0..4].copy_from_slice(&seq.to_le_bytes());
    e.0[28..32].copy_from_slice(&crc32(&seq.to_le_bytes()).to_le_bytes());

    let addr = OTADATA + sector * SECTOR;
    flash
        .erase(addr, addr + SECTOR)
        .map_err(|_| "otadata erase failed")?;
    flash
        .write(addr, &e.0)
        .map_err(|_| "otadata write failed")?;
    println!("ota: boot slot ota_{} (seq {})", slot, seq);
    Ok(())
}

struct Request {
    content_length: usize,
    expect_continue: bool,
}

// Reads until end of headers; returns the parsed request and how many body bytes were read
async fn read_head(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<(Request, usize, usize), &'static str> {
    let mut len = 0;
    let end = loop {
        if len == buf.len() {
            return Err("headers too large");
        }
        match socket.read(&mut buf[len..]).await {
            Ok(0) | Err(_) => return Err("connection closed"),
            Ok(n) => len += n,
        }
        if let Some(i) = buf[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };

    let head = core::str::from_utf8(&buf[..end]).map_err(|_| "bad headers")?;
    let mut lines = head.lines();
    if !lines.next().unwrap_or("").starts_with("POST /ota ") {
        return Err("expected POST /ota");
    }
    let mut req = Request {
        content_length: 0,
        expect_continue: false,
    };
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("content-length") {
            req.content_length = v.parse().map_err(|_| "bad content-length")?;
        } else if k.eq_ignore_ascii_case("expect") && v.eq_ignore_ascii_case("100-continue") {
            req.expect_continue = true;
        }
    }
    Ok((req, end, len))
}

async fn read_exact(
    socket: &mut TcpSocket<'_>,
    pending: &mut &[u8],
    buf: &mut [u8],
) -> Result<(), &'static str> {
    let n = pending.len().min(buf.len());
    buf[..n].copy_from_slice(&pending[..n]);
    *pending = &pending[n..];
    let mut pos = n;
    while pos < buf.len() {
        match socket.read(&mut buf[pos..]).await {
            Ok(0) | Err(_) => return Err("upload truncated"),
            Ok(n) => pos += n,
        }
    }
    Ok(())
}

async fn update(
    socket: &mut TcpSocket<'_>,
    head: &mut [u8; 1024],
    sector: &mut Aligned<{ SECTOR as usize }>,
) -> Result<(), &'static str> {
    let (req, body_start, head_len) = read_head(socket, head).await?;

    if req.content_length <= SIG_LEN {
        return Err("missing image");
    }
    let image_len = (req.content_length - SIG_LEN) as u32;
    if image_len > SLOT_SIZE {
        return Err("image larger than slot");
    }

    let slot = 1 - running_slot().ok_or("running slot unknown")?;
    let base = SLOTS[slot];
    println!("ota: receiving {} bytes into ota_{}", image_len, slot);

    if req.expect_continue {
        socket
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .ok();
    }

    let mut flash = FlashStorage::new();
    let mut pending = &head[body_start..head_len];
    let mut off = 0u32;
    while off < image_len {
        let n = (image_len - off).min(SECTOR) as usize;
        read_exact(socket, &mut pending, &mut sector.0[..n]).await?;
        if off == 0 {
            let chip = u16::from_le_bytes([sector.0[12], sector.0[13]]);
            if sector.0[0] != IMAGE_MAGIC || chip != CHIP_ID_ESP32C3 {
                return Err("not an esp32c3 app image");
            }
        }
        // Pad the tail to word alignment; erased flash reads 0xFF anyway
        let padded = (n + 3) & !3;
        sector.0[n..padded].fill(0xFF);
        flash
            .erase(base + off, base + off + SECTOR)
            .map_err(|_| "flash erase failed")?;
        flash
            .write(base + off, &sector.0[..padded])
            .map_err(|_| "flash write failed")?;
        off += n as u32;
        if off % 0x10000 == 0 {
            println!("ota: {} / {}", off, image_len);
        }
    }

    let mut sig = [0u8; SIG_LEN];
    read_exact(socket, &mut pending, &mut sig).await?;

    // Hash what actually landed in flash, not what came over the wire. Hashing takes ~2 s
    // at 80 MHz, so yield per sector to let the heartbeat task feed the watchdog.
    let mut hasher = Sha256::new();
    let mut off = 0u32;
    while off < image_len {
        let n = (image_len - off).min(SECTOR) as usize;
        let padded = (n + 3) & !3;
        flash
            .read(base + off, &mut sector.0[..padded])
            .map_err(|_| "flash read failed")?;
        hasher.update(&sector.0[..n]);
        off += n as u32;
        yield_now().await;
    }
    let digest = hasher.finalize();

    ed25519_compact::PublicKey::new(*PUBLIC_KEY)
        .verify(digest, &ed25519_compact::Signature::new(sig))
        .map_err(|_| "bad signature")?;
    println!("ota: signature ok");

    set_boot_slot(&mut flash, slot)
}

#[embassy_executor::task]
pub async fn ota_task(stack: &'static Stack<'static>) {
    let rx = OTA_RX.init([0u8; 8192]);
    let tx = OTA_TX.init([0u8; 512]);
    let head = HEAD_BUF.init([0u8; 1024]);
    let sector = SECTOR_BUF.init(Aligned([0u8; SECTOR as usize]));

    stack.wait_config_up().await;
    println!(
        "ota: listening on port {}, running from ota_{:?}",
        OTA_PORT,
        running_slot()
    );

    let mut socket = TcpSocket::new(*stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(10)));

    loop {
        if let Err(e) = socket.accept(OTA_PORT).await {
            println!("ota: accept error: {:?}", e);
            continue;
        }
        println!("ota: connection from {:?}", socket.remote_endpoint());

        let result = update(&mut socket, head, sector).await;
        match result {
            Ok(()) => send_response(&mut socket, "200 OK", "text/plain", b"ok, rebooting\n").await,
            Err(e) => {
                println!("ota: failed: {}", e);
                let mut body: String<64> = String::new();
                writeln!(body, "{}", e).ok();
                send_response(
                    &mut socket,
                    "400 Bad Request",
                    "text/plain",
                    body.as_bytes(),
                )
                .await;
            }
        }

        socket.flush().await.ok();
        socket.close();
        Timer::after(Duration::from_millis(200)).await;
        socket.abort();

        if result.is_ok() {
            esp_hal::reset::software_reset();
        }
    }
}
