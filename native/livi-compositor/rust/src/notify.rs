//! sd_notify by hand, READY=1 once the backend is up and WATCHDOG=1 from the
//! loop, over the datagram socket systemd names in NOTIFY_SOCKET. without
//! NOTIFY_SOCKET every call is a no-op.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::{Duration, Instant};

use crate::tuning::WATCHDOG_FRACTION;

pub struct Notify {
    sock: Option<(UnixDatagram, SocketAddr)>,
    interval: Option<Duration>,
    last: Instant,
}

impl Notify {
    pub fn from_env() -> Self {
        let sock = std::env::var("NOTIFY_SOCKET").ok().and_then(|p| {
            // a leading @ is the abstract namespace
            let addr = match p.strip_prefix('@') {
                Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
                None => SocketAddr::from_pathname(&p),
            };
            let addr = addr.map_err(|e| log::warn!("NOTIFY_SOCKET={p} unusable, {e}")).ok()?;
            let sock = UnixDatagram::unbound().map_err(|e| log::warn!("notify socket failed, {e}")).ok()?;
            Some((sock, addr))
        });
        let interval = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&us| us > 0)
            .map(|us| Duration::from_micros(us).mul_f64(WATCHDOG_FRACTION));
        Self { sock, interval, last: Instant::now() }
    }

    fn send(&self, msg: &str) {
        if let Some((sock, addr)) = &self.sock
            && let Err(e) = sock.send_to_addr(msg.as_bytes(), addr)
        {
            log::warn!("sd_notify {msg} failed, {e}");
        }
    }

    pub fn ready(&mut self) {
        self.send("READY=1");
        self.last = Instant::now();
    }

    /// from every loop turn, pings once the interval has passed.
    pub fn tick(&mut self) {
        let Some(interval) = self.interval else { return };
        if self.last.elapsed() >= interval {
            self.send("WATCHDOG=1");
            self.last = Instant::now();
        }
    }
}
