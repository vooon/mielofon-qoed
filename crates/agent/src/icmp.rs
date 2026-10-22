//! Continuous ICMPv6 echo probing, one task per managed link.
//!
//! Each link gets a raw ICMPv6 socket pinned to its tunnel interface
//! (`SO_BINDTODEVICE`) and connected to the OSPF neighbour's link-local
//! address with the interface scope, so neither the echo nor the reply can
//! take the path OSPF prefers to the peer's global address — the probe
//! measures this tunnel and nothing else. Without a neighbour row (adjacency
//! down, older rpcd-mod-bird) it falls back to the global /127 pair, still
//! pinned to the interface. One echo per `ping_interval`; a reply that has
//! not arrived by the next tick is a loss. RTT comes from the kernel receive
//! timestamp (`SO_TIMESTAMPNS`), falling back to the monotonic clock.
//!
//! While a throughput test runs (and a short grace after it) pings are paused:
//! the test loads the node's uplink, and its self-inflicted queueing must be
//! recorded neither as RTT nor as loss.

use crate::discover::Link;
use crate::window::{select_rtt, Snapshot, Window};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddrV6;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const ICMP6_ECHO_REQUEST: u8 = 128;
const ICMP6_ECHO_REPLY: u8 = 129;
/// `ICMP6_FILTER` socket option (netinet/icmp6.h), level `IPPROTO_ICMPV6`.
const ICMP6_FILTER: libc::c_int = 1;

/// Pauses pings around throughput tests.
#[derive(Default)]
pub struct Gate {
    running: AtomicBool,
    until: Mutex<Option<Instant>>,
}

impl Gate {
    pub fn paused(&self) -> bool {
        self.running.load(Ordering::Acquire)
            || self
                .until
                .lock()
                .expect("gate lock")
                .is_some_and(|t| Instant::now() < t)
    }

    pub fn begin(&self) {
        self.running.store(true, Ordering::Release);
    }

    /// End a test; pings stay paused for `grace` so the queue drains.
    pub fn end(&self, grace: Duration) {
        *self.until.lock().expect("gate lock") = Some(Instant::now() + grace);
        self.running.store(false, Ordering::Release);
    }
}

/// Per-link statistics exposed to replies and metrics.
#[derive(Debug, Clone)]
pub struct LinkStats {
    pub snapshot: Snapshot,
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
}

struct Entry {
    link: Link,
    window: Arc<Mutex<Window>>,
    task: JoinHandle<()>,
}

pub struct Prober {
    interval: Duration,
    window_size: usize,
    gate: Arc<Gate>,
    links: Mutex<HashMap<String, Entry>>,
    next_id: AtomicU16,
}

impl Prober {
    pub fn new(interval: Duration, window_size: usize, gate: Arc<Gate>) -> Self {
        Prober {
            interval,
            window_size,
            gate,
            links: Mutex::new(HashMap::new()),
            // Distinct echo ids per link socket; seeded from the pid so a
            // restarted agent does not match its predecessor's replies.
            next_id: AtomicU16::new(std::process::id() as u16),
        }
    }

    /// Start probing new links, stop removed ones, restart changed ones. A
    /// changed link (the neighbour's address came or went) keeps its window,
    /// so an adjacency flap does not wipe the loss it caused.
    pub fn reconcile(&self, links: &[Link]) {
        let mut map = self.links.lock().expect("prober lock");
        let mut windows: HashMap<String, Arc<Mutex<Window>>> = HashMap::new();
        map.retain(
            |iface, e| match links.iter().find(|l| &l.interface == iface) {
                Some(l) if l == &e.link => true,
                Some(_) => {
                    info!("probe {iface}: restarting (link changed)");
                    e.task.abort();
                    windows.insert(iface.clone(), e.window.clone());
                    false
                }
                None => {
                    info!("probe {iface}: stopped");
                    e.task.abort();
                    false
                }
            },
        );
        for l in links {
            if map.contains_key(&l.interface) {
                continue;
            }
            let window = windows
                .remove(&l.interface)
                .unwrap_or_else(|| Arc::new(Mutex::new(Window::new(self.window_size))));
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let task = tokio::spawn(ping_loop(
                l.clone(),
                window.clone(),
                self.gate.clone(),
                self.interval,
                id,
            ));
            match l.peer_ll {
                Some(ll) => info!(
                    "probe {}: -> {ll}%{} (link-local)",
                    l.interface, l.interface
                ),
                None => info!("probe {}: {} -> {}", l.interface, l.source, l.target),
            }
            map.insert(
                l.interface.clone(),
                Entry {
                    link: l.clone(),
                    window,
                    task,
                },
            );
        }
    }

    pub fn stats(&self, iface: &str) -> Option<LinkStats> {
        let map = self.links.lock().expect("prober lock");
        map.get(iface).map(|e| stats_of(&e.window))
    }
}

impl Drop for Prober {
    fn drop(&mut self) {
        if let Ok(map) = self.links.lock() {
            for e in map.values() {
                e.task.abort();
            }
        }
    }
}

fn stats_of(w: &Mutex<Window>) -> LinkStats {
    let w = w.lock().expect("window lock");
    LinkStats {
        snapshot: w.snapshot(),
        sent: w.sent,
        received: w.received,
        lost: w.lost,
    }
}

fn realtime_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

fn if_index(name: &str) -> io::Result<u32> {
    let c = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name"))?;
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => Err(io::Error::last_os_error()),
        idx => Ok(idx),
    }
}

fn open_socket(link: &Link) -> io::Result<AsyncFd<Socket>> {
    let sock = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    sock.set_nonblocking(true)?;
    // Egress pinned to the tunnel: the probe measures this link, never the
    // route OSPF currently prefers to the peer's address.
    sock.bind_device(Some(link.interface.as_bytes()))?;
    match link.peer_ll {
        // Link-local target with the interface scope: the reply is link-local
        // too, so it cannot come back any other way. The kernel picks our
        // link-local on the interface as the source.
        Some(ll) => {
            let scope = if_index(&link.interface)?;
            sock.connect(&SockAddr::from(SocketAddrV6::new(ll, 0, 0, scope)))?;
        }
        None => {
            sock.bind(&SockAddr::from(SocketAddrV6::new(link.source, 0, 0, 0)))?;
            sock.connect(&SockAddr::from(SocketAddrV6::new(link.target, 0, 0, 0)))?;
        }
    }

    let fd = sock.as_raw_fd();
    let on: libc::c_int = 1;
    // SAFETY: valid fd, option value is a c_int of the given size.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPNS,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        warn!(
            "probe {}: SO_TIMESTAMPNS: {}",
            link.interface,
            io::Error::last_os_error()
        );
    }

    // Only echo replies reach the socket (a set bit blocks that type).
    let mut filter = [u32::MAX; 8];
    filter[usize::from(ICMP6_ECHO_REPLY >> 5)] &= !(1u32 << (ICMP6_ECHO_REPLY & 31));
    // SAFETY: valid fd; the filter is the kernel's 256-bit icmp6_filter.
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_ICMPV6,
            ICMP6_FILTER,
            filter.as_ptr().cast(),
            std::mem::size_of_val(&filter) as libc::socklen_t,
        );
    }

    // SAFETY: `sock` owns its descriptor for the whole life of the AsyncFd
    // (I/O safety contract of `register`).
    unsafe { AsyncFd::register(sock) }.map_err(|e| e.into_parts().1)
}

/// Read one datagram; returns its length and the kernel RX stamp (ns).
fn recv_stamped(fd: i32, buf: &mut [u8]) -> io::Result<(usize, Option<i128>)> {
    let mut cmsg = [0u64; 16];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: zeroed msghdr is a valid initial state; all pointers below
    // reference live buffers for the duration of the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cmsg) as _;

    // SAFETY: fd is a valid socket, msg points at valid buffers.
    let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut stamp = None;
    // SAFETY: walking the control buffer the kernel just filled, using the
    // libc CMSG_* accessors bounded by msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            let hdr = &*c;
            if hdr.cmsg_level == libc::SOL_SOCKET
                && hdr.cmsg_type == libc::SCM_TIMESTAMPNS
                && hdr.cmsg_len as usize
                    >= libc::CMSG_LEN(std::mem::size_of::<libc::timespec>() as u32) as usize
            {
                let ts: libc::timespec = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast());
                stamp = Some(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128);
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n as usize, stamp))
}

/// Wait until `deadline` for the echo reply `(id, seq)`. `Ok(None)` = timeout.
async fn await_reply(
    sock: &AsyncFd<Socket>,
    id: u16,
    seq: u16,
    deadline: tokio::time::Instant,
) -> io::Result<Option<Option<i128>>> {
    let mut buf = [0u8; 512];
    loop {
        let Ok(guard) = tokio::time::timeout_at(deadline, sock.readable()).await else {
            return Ok(None);
        };
        let mut guard = guard?;
        match guard.try_io(|inner| recv_stamped(inner.get_ref().as_raw_fd(), &mut buf)) {
            Ok(Ok((n, stamp))) => {
                if n >= 8
                    && buf[0] == ICMP6_ECHO_REPLY
                    && u16::from_be_bytes([buf[4], buf[5]]) == id
                    && u16::from_be_bytes([buf[6], buf[7]]) == seq
                {
                    return Ok(Some(stamp));
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
}

async fn ping_loop(
    link: Link,
    window: Arc<Mutex<Window>>,
    gate: Arc<Gate>,
    interval: Duration,
    id: u16,
) {
    let mut seq: u16 = 0;
    loop {
        let sock = match open_socket(&link) {
            Ok(s) => s,
            Err(e) => {
                // Typically the tunnel address is not assigned yet.
                warn!("probe {}: open socket: {e}; retrying", link.interface);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut send_failing = false;

        loop {
            let tick = ticker.tick().await;
            if gate.paused() {
                continue;
            }

            seq = seq.wrapping_add(1);
            let mut pkt = [0u8; 16];
            pkt[0] = ICMP6_ECHO_REQUEST;
            pkt[4..6].copy_from_slice(&id.to_be_bytes());
            pkt[6..8].copy_from_slice(&seq.to_be_bytes());

            let sent_mono = Instant::now();
            let sent = sock.get_ref().send(&pkt);
            let sent_rt = realtime_ns();
            window.lock().expect("window lock").sent += 1;

            if let Err(e) = sent {
                // Unreachable tunnel: a loss, not a silent gap.
                if !send_failing {
                    warn!("probe {}: send: {e}", link.interface);
                    send_failing = true;
                }
                window.lock().expect("window lock").record_timeout();
                continue;
            }
            if send_failing {
                info!("probe {}: send recovered", link.interface);
                send_failing = false;
            }

            let reply = await_reply(&sock, id, seq, tick + interval).await;
            if gate.paused() {
                // A throughput test started while we waited: not a sample.
                continue;
            }
            let mut w = window.lock().expect("window lock");
            match reply {
                Ok(Some(stamp)) => {
                    let rtt = select_rtt(stamp.map(|rx| rx - sent_rt), sent_mono.elapsed());
                    w.record_reply(rtt);
                }
                Ok(None) => w.record_timeout(),
                Err(e) => {
                    drop(w);
                    warn!("probe {}: recv: {e}; reopening", link.interface);
                    break;
                }
            }
        }
        debug!("probe {}: socket reopen", link.interface);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn gate_pauses_during_and_after_a_test() {
        let g = Gate::default();
        assert!(!g.paused());
        g.begin();
        assert!(g.paused());
        g.end(Duration::from_millis(50));
        assert!(g.paused(), "grace period");
        std::thread::sleep(Duration::from_millis(80));
        assert!(!g.paused());
    }

    /// Needs CAP_NET_RAW: pings ::1 through the real socket path, kernel
    /// timestamps included. Run with `cargo test -- --ignored` as root.
    #[tokio::test]
    #[ignore]
    async fn pings_loopback() {
        let link = Link {
            interface: "lo".into(),
            to: "self".into(),
            source: Ipv6Addr::LOCALHOST,
            target: Ipv6Addr::LOCALHOST,
            peer_ll: None,
            cost: None,
            peer_rid: None,
        };
        let p = Prober::new(Duration::from_millis(100), 60, Arc::new(Gate::default()));
        p.reconcile(std::slice::from_ref(&link));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let s = p.stats("lo").unwrap();
        assert!(s.received >= 5, "{s:?}");
        assert!(s.snapshot.rtt_ms.unwrap() < 5.0);
        assert_eq!(s.snapshot.loss_pct, Some(0.0));

        // The scoped-target path (neighbour address known): the socket is
        // connected with the interface index and the kernel picks the source.
        // A changed link restarts the task on the same window.
        let scoped = Link {
            peer_ll: Some(Ipv6Addr::LOCALHOST),
            cost: None,
            ..link
        };
        p.reconcile(&[scoped]);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let s2 = p.stats("lo").unwrap();
        assert!(s2.received > s.received + 5, "{s2:?} after {s:?}");
        assert_eq!(s2.snapshot.loss_pct, Some(0.0));
    }

    /// Needs CAP_NET_RAW (see `pings_loopback`): the reply carries a kernel
    /// RX timestamp and it is parsed out of the control messages.
    #[tokio::test]
    #[ignore]
    async fn reply_has_kernel_timestamp() {
        let link = Link {
            interface: "lo".into(),
            to: "self".into(),
            source: Ipv6Addr::LOCALHOST,
            target: Ipv6Addr::LOCALHOST,
            peer_ll: None,
            cost: None,
            peer_rid: None,
        };
        let sock = open_socket(&link).unwrap();
        let mut pkt = [0u8; 16];
        pkt[0] = ICMP6_ECHO_REQUEST;
        pkt[4..6].copy_from_slice(&7u16.to_be_bytes());
        pkt[6..8].copy_from_slice(&1u16.to_be_bytes());
        let sent = realtime_ns();
        sock.get_ref().send(&pkt).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let stamp = await_reply(&sock, 7, 1, deadline).await.unwrap().unwrap();
        let rx = stamp.expect("kernel RX timestamp");
        assert!(
            rx >= sent - 1_000_000 && rx - sent < 1_000_000_000,
            "{rx} vs {sent}"
        );
    }
}
