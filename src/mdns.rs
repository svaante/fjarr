use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    IpAddress, IpEndpoint, Stack,
};
use embassy_time::{with_timeout, Duration, Timer};
use esp_println::println;
use static_cell::StaticCell;

pub const HOSTNAME: &str = "fjarr";
const MDNS_PORT: u16 = 5353;
const TTL_SECS: u32 = 4500;
// Re-announce interval; keeps clients from losing us when their cache expires
// and ensures we recover quickly after a reconnect detection cycle.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60);

fn encode_name(labels: &[&str], buf: &mut [u8], mut pos: usize) -> usize {
    for label in labels {
        let len = label.len();
        buf[pos] = len as u8;
        pos += 1;
        buf[pos..pos + len].copy_from_slice(label.as_bytes());
        pos += len;
    }
    buf[pos] = 0;
    pos + 1
}

fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        if pos >= buf.len() {
            return None;
        }
        let b = buf[pos];
        if b == 0 {
            return Some(pos + 1);
        }
        if b & 0xC0 == 0xC0 {
            return Some(pos + 2);
        }
        pos += 1 + b as usize;
    }
}

fn name_is_ours(buf: &[u8], mut pos: usize) -> bool {
    let hb = HOSTNAME.as_bytes();
    let hlen = hb.len();
    if pos + 1 + hlen + 1 + 5 + 1 > buf.len() {
        return false;
    }
    if buf[pos] as usize != hlen {
        return false;
    }
    pos += 1;
    if !buf[pos..pos + hlen].eq_ignore_ascii_case(hb) {
        return false;
    }
    pos += hlen;
    if buf[pos] != 5 {
        return false;
    }
    pos += 1;
    if !buf[pos..pos + 5].eq_ignore_ascii_case(b"local") {
        return false;
    }
    buf[pos + 5] == 0
}

fn build_response(ip: [u8; 4], buf: &mut [u8]) -> usize {
    buf[..12].copy_from_slice(&[
        0x00, 0x00, // id: 0
        0x84, 0x00, // flags: authoritative response
        0x00, 0x00, // qdcount: 0
        0x00, 0x01, // ancount: 1
        0x00, 0x00, // nscount: 0
        0x00, 0x00, // arcount: 0
    ]);
    let mut pos = encode_name(&[HOSTNAME, "local"], buf, 12);
    buf[pos..pos + 2].copy_from_slice(&[0x00, 0x01]);
    pos += 2; // type: A
    buf[pos..pos + 2].copy_from_slice(&[0x80, 0x01]);
    pos += 2; // class: IN + cache-flush
    buf[pos..pos + 4].copy_from_slice(&TTL_SECS.to_be_bytes());
    pos += 4; // TTL
    buf[pos..pos + 2].copy_from_slice(&[0x00, 0x04]);
    pos += 2; // rdlength: 4
    buf[pos..pos + 4].copy_from_slice(&ip);
    pos + 4
}

fn handle_query(pkt: &[u8], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    if pkt.len() < 12 || pkt[2] & 0x80 != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let mut pos = 12;
    for _ in 0..qdcount {
        let name_start = pos;
        pos = skip_name(pkt, pos)?;
        if pos + 4 > pkt.len() {
            return None;
        }
        let qtype = u16::from_be_bytes([pkt[pos], pkt[pos + 1]]);
        pos += 4;
        if (qtype == 1 || qtype == 255) && name_is_ours(pkt, name_start) {
            return Some(build_response(ip, out));
        }
    }
    None
}

static RX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
static RX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
static TX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

#[embassy_executor::task]
pub async fn mdns_task(stack: &'static Stack<'static>) {
    stack.wait_config_up().await;

    let mdns_group = IpAddress::v4(224, 0, 0, 251);
    let mdns_ep = IpEndpoint::new(mdns_group, MDNS_PORT);

    let mut socket = UdpSocket::new(
        *stack,
        RX_META.init([PacketMetadata::EMPTY; 4]),
        RX_BUF.init([0u8; 1024]),
        TX_META.init([PacketMetadata::EMPTY; 4]),
        TX_BUF.init([0u8; 512]),
    );
    socket.bind(MDNS_PORT).unwrap();
    println!("mdns: listening on UDP port {}", MDNS_PORT);

    let mut pkt = [0u8; 512];
    let mut out = [0u8; 256];
    // Track last seen IP; None means disconnected.
    let mut last_ip: Option<[u8; 4]> = None;

    loop {
        let ip = stack.config_v4().map(|c| c.address.address().octets());

        if ip != last_ip {
            last_ip = ip;
            if let Some(ip) = ip {
                // Re-join multicast; membership is lost when the WiFi
                // interface goes down and the driver resets its group table.
                match stack.join_multicast_group(mdns_group) {
                    Ok(_) => println!("mdns: joined multicast group 224.0.0.251"),
                    Err(e) => println!("mdns: multicast join failed: {:?}", e),
                }
                let n = build_response(ip, &mut out);
                socket.send_to(&out[..n], mdns_ep).await.ok();
                println!(
                    "mdns: announced {}.local -> {}.{}.{}.{}",
                    HOSTNAME, ip[0], ip[1], ip[2], ip[3]
                );
            }
        }

        match with_timeout(ANNOUNCE_INTERVAL, socket.recv_from(&mut pkt)).await {
            Ok(Ok((n, src))) => {
                if let Some(ip) = last_ip {
                    if let Some(rn) = handle_query(&pkt[..n], ip, &mut out) {
                        println!(
                            "mdns: query from {:?} - replying with {}.local",
                            src, HOSTNAME
                        );
                        socket.send_to(&out[..rn], mdns_ep).await.ok();
                    }
                }
            }
            Ok(Err(e)) => {
                println!("mdns: recv error: {:?}", e);
                Timer::after(Duration::from_millis(100)).await;
            }
            Err(_timeout) => {
                // Periodic re-announce so clients with stale caches recover.
                if let Some(ip) = last_ip {
                    let n = build_response(ip, &mut out);
                    socket.send_to(&out[..n], mdns_ep).await.ok();
                    println!("mdns: re-announced {}.local", HOSTNAME);
                }
            }
        }
    }
}
