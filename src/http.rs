use core::fmt::Write as FmtWrite;
use embedded_io_async::Write;

use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::Duration;
use esp_println::println;
use heapless::String;
use static_cell::StaticCell;

pub const HTTP_TASK_COUNT: usize = 3;

static TLS_RX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
static TLS_TX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
static HTTP_RX: [StaticCell<[u8; 1024]>; HTTP_TASK_COUNT] =
    [StaticCell::new(), StaticCell::new(), StaticCell::new()];
static HTTP_TX: [StaticCell<[u8; 4096]>; HTTP_TASK_COUNT] =
    [StaticCell::new(), StaticCell::new(), StaticCell::new()];

const HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));
const FAVICON: &[u8] = include_bytes!("favicon.svg");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/apple-touch-icon.png"));

async fn send_response_gzip(socket: &mut TcpSocket<'_>, body: &[u8]) {
    let mut cl: String<8> = String::new();
    write!(cl, "{}", body.len()).ok();

    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Encoding: gzip\r\nContent-Length: ").await.ok();
    socket.write_all(cl.as_bytes()).await.ok();
    socket
        .write_all(b"\r\nConnection: close\r\n\r\n")
        .await
        .ok();
    socket.write_all(body).await.ok();
}

async fn send_response(socket: &mut TcpSocket<'_>, status: &str, content_type: &str, body: &[u8]) {
    let mut cl: String<8> = String::new();
    write!(cl, "{}", body.len()).ok();

    socket.write_all(b"HTTP/1.1 ").await.ok();
    socket.write_all(status.as_bytes()).await.ok();
    socket.write_all(b"\r\nContent-Type: ").await.ok();
    socket.write_all(content_type.as_bytes()).await.ok();
    socket.write_all(b"\r\nContent-Length: ").await.ok();
    socket.write_all(cl.as_bytes()).await.ok();
    socket
        .write_all(b"\r\nConnection: close\r\n\r\n")
        .await
        .ok();
    socket.write_all(body).await.ok();
}

fn qparam<'a>(q: &'a str, k: &str) -> Option<&'a str> {
    q.split('&')
        .find_map(|p| p.strip_prefix(k)?.strip_prefix('='))
}

async fn handle(socket: &mut TcpSocket<'_>, buf: &[u8]) {
    let req = core::str::from_utf8(buf).unwrap_or("");
    let path = req
        .lines()
        .next()
        .unwrap_or("")
        .split(' ')
        .nth(1)
        .unwrap_or("/");
    let (route, query) = match path.find('?') {
        Some(i) => (&path[1..i], &path[i + 1..]),
        None => (&path[1..], ""),
    };

    if route.is_empty() {
        send_response_gzip(socket, HTML_GZ).await;
        return;
    }

    if route == "favicon.svg" {
        send_response(socket, "200 OK", "image/svg+xml", FAVICON).await;
        return;
    }

    if route.starts_with("apple-touch-icon") && route.ends_with(".png") {
        send_response(socket, "200 OK", "image/png", APPLE_TOUCH_ICON).await;
        return;
    }

    let mut out: heapless::String<1200> = heapless::String::new();
    crate::cmd::dispatch(
        route,
        qparam(query, "name"),
        qparam(query, "device"),
        &mut out,
    )
    .await;
    if !out.is_empty() {
        send_response(socket, "200 OK", "application/json", out.as_bytes()).await;
    } else {
        send_response(socket, "200 OK", "text/plain", b"ok").await;
    }
}

#[embassy_executor::task]
pub async fn https_reject_task(stack: &'static Stack<'static>) {
    stack.wait_config_up().await;

    let mut socket = TcpSocket::new(
        *stack,
        TLS_RX_BUF.init([0u8; 16]),
        TLS_TX_BUF.init([0u8; 16]),
    );

    loop {
        match socket.accept(443u16).await {
            Ok(()) => {
                println!(
                    "https: connection from {:?} - rejected",
                    socket.remote_endpoint()
                );
                socket.abort();
            }
            Err(e) => println!("https: accept error: {:?}", e),
        }
    }
}

#[embassy_executor::task(pool_size = HTTP_TASK_COUNT)]
pub async fn http_task(stack: &'static Stack<'static>, idx: usize) {
    let rx = HTTP_RX[idx].init([0u8; 1024]);
    let tx = HTTP_TX[idx].init([0u8; 4096]);

    stack.wait_config_up().await;
    println!("http: listening on port 80");

    let mut socket = TcpSocket::new(*stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(5)));

    let mut req_buf = [0u8; 1024];

    loop {
        if let Err(e) = socket.accept(80u16).await {
            println!("http: accept error: {:?}", e);
            continue;
        }

        println!("http: connection from {:?}", socket.remote_endpoint());

        let n = match socket.read(&mut req_buf).await {
            Ok(0) | Err(_) => {
                socket.abort();
                continue;
            }
            Ok(n) => n,
        };

        let first_line = core::str::from_utf8(&req_buf[..n])
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!("http: {} bytes - {}", n, first_line);

        handle(&mut socket, &req_buf[..n]).await;

        socket.flush().await.ok();
        socket.close();
        socket.abort();
    }
}
