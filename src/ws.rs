use core::fmt::Write as FmtWrite;
use embedded_io_async::Write;

use embassy_futures::select::{select, Either};
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{Duration, Timer};
use esp_println::println;
use heapless::String;
use static_cell::StaticCell;

use crate::recording::{RecordingState, RECORDING};

static WS_RX_A: StaticCell<[u8; 512]> = StaticCell::new();
static WS_TX_A: StaticCell<[u8; 2048]> = StaticCell::new();
static WS_RX_B: StaticCell<[u8; 512]> = StaticCell::new();
static WS_TX_B: StaticCell<[u8; 2048]> = StaticCell::new();

#[derive(PartialEq)]
enum State {
    Idle,
    Busy,
    Done(usize),
}

async fn send_handshake(socket: &mut TcpSocket<'_>, key: &str) {
    let mut sha = sha1_smol::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");

    use base64::Engine as _;
    let d = sha.digest().bytes();
    let mut accept = [0u8; 28];
    base64::engine::general_purpose::STANDARD
        .encode_slice(d, &mut accept)
        .ok();

    socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ").await.ok();
    socket.write_all(&accept).await.ok();
    socket.write_all(b"\r\n\r\n").await.ok();
    socket.flush().await.ok();
}

async fn ws_send(socket: &mut TcpSocket<'_>, payload: &[u8]) {
    let n = payload.len();
    if n < 126 {
        socket.write_all(&[0x81, n as u8]).await.ok();
    } else {
        socket
            .write_all(&[0x81, 126, (n >> 8) as u8, n as u8])
            .await
            .ok();
    }
    socket.write_all(payload).await.ok();
    socket.flush().await.ok();
}

fn parse_frame<'a>(buf: &'a mut [u8], n: usize) -> Option<(u8, &'a [u8])> {
    const HEADER_LEN: usize = 6; // opcode + len + 4-byte mask
    const MAX_SIMPLE_LEN: usize = 125; // >= 126 means extended length encoding
    const OPCODE_MASK: u8 = 0x0F;
    const LEN_MASK: u8 = 0x7F;

    let pay_len = (buf[1] & LEN_MASK) as usize;
    if n < HEADER_LEN + pay_len || pay_len > MAX_SIMPLE_LEN {
        return None;
    }

    let mask = [buf[2], buf[3], buf[4], buf[5]];
    for i in 0..pay_len {
        buf[HEADER_LEN + i] ^= mask[i % 4];
    }

    Some((buf[0] & OPCODE_MASK, &buf[HEADER_LEN..HEADER_LEN + pay_len]))
}

async fn current_state() -> State {
    let rec = RECORDING.lock().await;
    match &*rec {
        RecordingState::Idle => State::Idle,
        RecordingState::Capturing { .. } => State::Busy,
        RecordingState::Done { pulses } => State::Done(pulses.len()),
    }
}

fn json_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let mut pat: String<16> = String::new();
    write!(pat, "\"{}\":\"", key).ok();
    let i = s.find(pat.as_str())? + pat.len();
    Some(&s[i..i + s[i..].find('"')?])
}

async fn push_state(socket: &mut TcpSocket<'_>, state: &State) {
    match state {
        State::Idle => ws_send(socket, br#"{"type":"idle"}"#).await,
        State::Busy => ws_send(socket, br#"{"type":"busy"}"#).await,
        State::Done(p) => {
            let mut msg: String<32> = String::new();
            write!(msg, r#"{{"type":"done","pulses":{}}}"#, p).ok();
            ws_send(socket, msg.as_bytes()).await;
        }
    }
}

async fn handle_cmd(socket: &mut TcpSocket<'_>, msg: &[u8]) {
    let Ok(text) = core::str::from_utf8(msg) else {
        return;
    };
    let Some(typ) = json_str(text, "type") else {
        return;
    };
    let mut out: String<1200> = String::new();
    crate::cmd::dispatch(
        typ,
        json_str(text, "name"),
        json_str(text, "device"),
        &mut out,
    )
    .await;
    if !out.is_empty() {
        ws_send(socket, out.as_bytes()).await;
    }
}

async fn ws_session(socket: &mut TcpSocket<'_>, key: &str) {
    send_handshake(socket, key).await;

    let mut known = current_state().await;
    push_state(socket, &known).await;
    let mut out: String<1200> = String::new();
    crate::cmd::dispatch("list", None, None, &mut out).await;
    ws_send(socket, out.as_bytes()).await;

    let mut buf = [0u8; 256];
    loop {
        match select(
            socket.read(&mut buf),
            Timer::after(Duration::from_millis(50)),
        )
        .await
        {
            Either::First(Ok(0)) | Either::First(Err(_)) => break,
            Either::First(Ok(n)) => {
                if let Some((opcode, payload)) = parse_frame(&mut buf, n) {
                    match opcode {
                        8 => break,
                        9 => {
                            socket.write_all(&[0x8A, 0x00]).await.ok();
                            socket.flush().await.ok();
                        }
                        _ => handle_cmd(socket, payload).await,
                    }
                }
            }
            Either::Second(_) => {}
        }

        let new = current_state().await;
        if new != known {
            push_state(socket, &new).await;
            known = new;
        }
    }
}

#[embassy_executor::task]
pub async fn ws_task(stack: &'static Stack<'static>) {
    stack.wait_config_up().await;
    println!("ws: listening on port 81");

    let mut sa = TcpSocket::new(*stack, WS_RX_A.init([0u8; 512]), WS_TX_A.init([0u8; 2048]));
    let mut sb = TcpSocket::new(*stack, WS_RX_B.init([0u8; 512]), WS_TX_B.init([0u8; 2048]));
    let mut req = [0u8; 512];

    // Wait for first client on either socket.
    match select(sa.accept(81u16), sb.accept(81u16)).await {
        Either::First(_) => {}
        Either::Second(_) => core::mem::swap(&mut sa, &mut sb),
    }

    loop {
        let n = sa.read(&mut req).await.unwrap_or(0);
        let text = core::str::from_utf8(&req[..n]).unwrap_or("");
        let maybe_sec_key = text
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key:").map(str::trim));

        if let Some(k) = maybe_sec_key {
            let mut ks: String<64> = String::new();
            ks.push_str(k).ok();

            // Run session on socket a and accept the next client on
            // socket b. On new connection kick and swap.
            match select(ws_session(&mut sa, ks.as_str()), sb.accept(81u16)).await {
                Either::First(_) => {
                    // Session ended naturally: wait for next client
                    // on either socket.
                    sa.abort();
                    match select(sa.accept(81u16), sb.accept(81u16)).await {
                        Either::First(_) => {}
                        Either::Second(_) => core::mem::swap(&mut sa, &mut sb),
                    }
                }
                Either::Second(_) => {
                    // New client arrived on b: kick the old session and promote b.
                    sa.abort();
                    core::mem::swap(&mut sa, &mut sb);
                    println!("ws: new client, kicked previous session");
                }
            }
        } else {
            sa.abort();
            match select(sa.accept(81u16), sb.accept(81u16)).await {
                Either::First(_) => {}
                Either::Second(_) => core::mem::swap(&mut sa, &mut sb),
            }
        }
    }
}
