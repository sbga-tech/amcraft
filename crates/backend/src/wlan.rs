//! The wlan socket: the UDP socket on which Wi-Fi units announce themselves (`spec/libam/allnet.md` part 11, 1.2).
//!
//! [`UdpAnnounce`] is the default implementation: a non-blocking UDP socket bound to [`ANNOUNCE_PORT`] on every
//! address.

/// UDP port the alwifi socket binds (`htons(0x9C4A)`).
pub const ANNOUNCE_PORT: u16 = 40010;

/// Largest datagram the poll accepts.
pub const MAX_DATAGRAM: usize = 2048;

/// The alwifi UDP socket as libam sees it: opened by `alwSetOption(6, {1, 0})`, closed by any other value.
pub trait WlanSocket: Send {
    /// Opens the socket (UDP, port [`ANNOUNCE_PORT`], any address). `false` when the OS refuses.
    fn open(&mut self) -> bool;

    /// Closes the socket.
    fn close(&mut self);

    /// One pending datagram (at most [`MAX_DATAGRAM`] bytes), `None` when nothing is readable.
    fn receive(&mut self) -> Option<Vec<u8>>;
}

/// The production socket: a non-blocking UDP socket bound to [`ANNOUNCE_PORT`] on every address.
#[derive(Debug, Default)]
pub struct UdpAnnounce(Option<std::net::UdpSocket>);

impl WlanSocket for UdpAnnounce {
    fn open(&mut self) -> bool {
        let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, ANNOUNCE_PORT))
            .and_then(|socket| socket.set_nonblocking(true).map(|()| socket));
        self.0 = socket.ok();
        self.0.is_some()
    }

    fn close(&mut self) {
        self.0 = None;
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        let mut buffer = [0u8; MAX_DATAGRAM];
        let (length, _) = self.0.as_ref()?.recv_from(&mut buffer).ok()?;
        Some(buffer[..length].to_vec())
    }
}
