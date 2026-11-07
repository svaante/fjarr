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

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_NSEC: u16 = 47;
const TYPE_ANY: u16 = 255;

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

// Matches HOSTNAME.local, following compression pointers; a second question
// in the same packet usually points back at the first one's name.
fn name_is_ours(buf: &[u8], mut pos: usize) -> bool {
    let expected: [&[u8]; 2] = [HOSTNAME.as_bytes(), b"local"];
    let mut label = 0;
    // Bound pointer hops so a looping pointer chain can't spin forever
    let mut hops = 0;
    loop {
        let Some(&len) = buf.get(pos) else {
            return false;
        };
        if len & 0xC0 == 0xC0 {
            let Some(&lo) = buf.get(pos + 1) else {
                return false;
            };
            hops += 1;
            if hops > 8 {
                return false;
            }
            pos = ((len as usize & 0x3F) << 8) | lo as usize;
            continue;
        }
        if len == 0 {
            return label == expected.len();
        }
        let len = len as usize;
        let Some(want) = expected.get(label) else {
            return false;
        };
        let Some(got) = buf.get(pos + 1..pos + 1 + len) else {
            return false;
        };
        if !got.eq_ignore_ascii_case(want) {
            return false;
        }
        label += 1;
        pos += 1 + len;
    }
}

#[derive(Clone, Copy)]
enum Record {
    A,
    // Asserts we have no AAAA (RFC 6762 §6.1); without it clients wait for
    // their AAAA query to time out before using the A record.
    Nsec,
}

fn write_record(rec: Record, ip: [u8; 4], buf: &mut [u8], mut pos: usize) -> usize {
    if pos == 12 {
        pos = encode_name(&[HOSTNAME, "local"], buf, pos);
    } else {
        // Pointer to the name written by the first record
        buf[pos..pos + 2].copy_from_slice(&[0xC0, 0x0C]);
        pos += 2;
    }
    let rtype = match rec {
        Record::A => TYPE_A,
        Record::Nsec => TYPE_NSEC,
    };
    buf[pos..pos + 2].copy_from_slice(&rtype.to_be_bytes());
    pos += 2; // type
    buf[pos..pos + 2].copy_from_slice(&[0x80, 0x01]);
    pos += 2; // class: IN + cache-flush
    buf[pos..pos + 4].copy_from_slice(&TTL_SECS.to_be_bytes());
    pos += 4; // TTL
    let rdlen_pos = pos;
    pos += 2; // rdlength: filled below
    match rec {
        Record::A => {
            buf[pos..pos + 4].copy_from_slice(&ip);
            pos += 4;
        }
        Record::Nsec => {
            // Next domain name: our own name, uncompressed
            pos = encode_name(&[HOSTNAME, "local"], buf, pos);
            // Type bitmap: window 0, 1 byte, only A (type 1) present
            buf[pos..pos + 3].copy_from_slice(&[0x00, 0x01, 0x40]);
            pos += 3;
        }
    }
    let rdlen = (pos - rdlen_pos - 2) as u16;
    buf[rdlen_pos..rdlen_pos + 2].copy_from_slice(&rdlen.to_be_bytes());
    pos
}

// Records answering a question go in the answer section; the other goes in
// the additional section so the client caches both in one round trip.
fn build_response(ip: [u8; 4], want_a: bool, want_aaaa: bool, buf: &mut [u8]) -> usize {
    let (records, ancount) = match (want_a, want_aaaa) {
        (true, true) => ([Record::A, Record::Nsec], 2u16),
        (false, true) => ([Record::Nsec, Record::A], 1),
        _ => ([Record::A, Record::Nsec], 1),
    };
    let arcount = 2 - ancount;
    buf[..12].copy_from_slice(&[
        0x00, 0x00, // id: 0
        0x84, 0x00, // flags: authoritative response
        0x00, 0x00, // qdcount: 0
        0x00, 0x00, // ancount: filled below
        0x00, 0x00, // nscount: 0
        0x00, 0x00, // arcount: filled below
    ]);
    buf[6..8].copy_from_slice(&ancount.to_be_bytes());
    buf[10..12].copy_from_slice(&arcount.to_be_bytes());
    let mut pos = 12;
    for rec in records {
        pos = write_record(rec, ip, buf, pos);
    }
    pos
}

fn build_announcement(ip: [u8; 4], buf: &mut [u8]) -> usize {
    build_response(ip, true, false, buf)
}

fn handle_query(pkt: &[u8], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    if pkt.len() < 12 || pkt[2] & 0x80 != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let mut pos = 12;
    let mut want_a = false;
    let mut want_aaaa = false;
    for _ in 0..qdcount {
        let name_start = pos;
        pos = skip_name(pkt, pos)?;
        if pos + 4 > pkt.len() {
            return None;
        }
        let qtype = u16::from_be_bytes([pkt[pos], pkt[pos + 1]]);
        pos += 4;
        if !matches!(qtype, TYPE_A | TYPE_AAAA | TYPE_ANY) || !name_is_ours(pkt, name_start) {
            continue;
        }
        want_a |= qtype != TYPE_AAAA;
        want_aaaa |= qtype != TYPE_A;
    }
    (want_a || want_aaaa).then(|| build_response(ip, want_a, want_aaaa, out))
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
                let n = build_announcement(ip, &mut out);
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
                    let n = build_announcement(ip, &mut out);
                    socket.send_to(&out[..n], mdns_ep).await.ok();
                    println!("mdns: re-announced {}.local", HOSTNAME);
                }
            }
        }
    }
}
