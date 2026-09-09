//! Just enough of the systemd notification protocol to be `Type=notify` with a
//! watchdog, without taking a dependency for it.
//!
//! The watchdog is the part that matters here: a fan daemon that wedges while
//! it is the one driving the SMC's fans leaves them stuck at whatever it last
//! commanded. Letting systemd notice and restart us — which runs the unit's
//! `ExecStopPost` and hands the fan back — is a much better failure than a
//! silent stall.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

pub struct Notifier {
    socket: Option<(UnixDatagram, SocketAddr)>,
    /// How often systemd expects to hear from us, already halved as the
    /// protocol recommends.
    pub watchdog_interval: Option<Duration>,
}

impl Notifier {
    /// Wire up from the environment. Absent `NOTIFY_SOCKET` — running by hand,
    /// or under a plain `Type=simple` unit — every method becomes a no-op.
    pub fn from_env() -> Notifier {
        let socket = std::env::var_os("NOTIFY_SOCKET").and_then(|raw| {
            let raw = raw.to_string_lossy().into_owned();
            let addr = match raw.strip_prefix(['@', '\0']) {
                Some(name) => SocketAddr::from_abstract_name(name.as_bytes()).ok()?,
                None => SocketAddr::from_pathname(&raw).ok()?,
            };
            Some((UnixDatagram::unbound().ok()?, addr))
        });

        let watchdog_interval = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&usec| usec > 0)
            // systemd asks to be pinged at least twice per interval.
            .map(|usec| Duration::from_micros(usec / 2));

        Notifier { socket, watchdog_interval }
    }

    fn send(&self, message: &str) {
        // A failed notification is not worth failing the daemon over, and not
        // worth logging every second either.
        if let Some((sock, addr)) = &self.socket {
            let _ = sock.send_to_addr(message.as_bytes(), addr);
        }
    }

    pub fn ready(&self) {
        self.send("READY=1");
    }

    pub fn watchdog(&self) {
        if self.watchdog_interval.is_some() {
            self.send("WATCHDOG=1");
        }
    }

    pub fn status(&self, status: &str) {
        self.send(&format!("STATUS={status}"));
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }
}
