//! The download-order context of the ALL.Net manager (`am::allnet::Context`, manager `+7848`; `0x14028FA40`
//! initialise, `0x14028FA90` stop, `0x14028FAB0` per-frame poll, `0x14028FC60` read the URI). It watches the alw
//! status kind 1, asks for a download order whenever the authentication finished or the last order failed long
//! enough ago, and keeps the `uri=` text NetDeliver reads as its key.

use crate::config::NetDeliverConfig;

use super::auth::Auth;

/// Seconds after which an answered or failed order is requested again (`0x708`).
const REFRESH_SECONDS: u64 = 1800;

/// The context state word (`+4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Stopped.
    #[default]
    Stopped,
    /// Waiting for the authentication or the order.
    Idle,
    /// The last order failed; the key is empty.
    Failed,
    /// The last order was answered; the key holds its URI.
    Answered,
}

/// `am::allnet::Context`.
#[derive(Debug, Clone, Default)]
pub struct DlOrder {
    /// `+0`: `netdeliver.enable`.
    enabled: bool,
    /// `+1`: the constant `netdeliver` byte; set, an answered order is repeated every half hour.
    refresh: bool,
    state: State,
    /// `+8`: the `uri=` text.
    key: String,
    /// `+536`: the key changed in the last poll.
    key_changed: bool,
    /// `+524`: the status kind 1 of the previous poll.
    last_kind: u32,
    /// `+528`: the stopwatch, in `tick_ms`.
    stamp: u64,
}

impl DlOrder {
    /// `0x14028FA40`: the context of the configured NetDeliver, state "idle".
    pub fn new(config: &NetDeliverConfig, now_ms: u64) -> Self {
        Self {
            enabled: config.enable,
            refresh: config.const_one,
            state: State::Idle,
            key: String::new(),
            key_changed: false,
            last_kind: 0,
            stamp: now_ms,
        }
    }

    /// `0x14028FA90`: the state returns to stopped; the key stays.
    pub fn stop(&mut self) {
        self.state = State::Stopped;
    }

    /// `0x14023BB10` (`state in {2, 3}`): an order finished, whatever its outcome.
    pub fn finished(&self) -> bool {
        matches!(self.state, State::Failed | State::Answered)
    }

    /// `0x14023BB50`: the key changed in the last poll.
    pub fn key_changed(&self) -> bool {
        self.key_changed
    }

    /// `0x14023BBC0`: the key while the context is enabled and running.
    pub fn key(&self) -> Option<&str> {
        (self.enabled && self.state != State::Stopped).then_some(self.key.as_str())
    }

    fn elapsed_seconds(&self, now_ms: u64) -> u64 {
        now_ms.wrapping_sub(self.stamp) / 1000
    }

    /// `0x14028FAB0`, once per Operating frame after the authentication ran.
    pub fn update(&mut self, now_ms: u64, auth: &mut Auth) {
        if !self.enabled {
            return;
        }
        self.key_changed = false;
        let previous = self.last_kind;
        let kind = auth.download_status();
        self.last_kind = kind;
        match kind {
            // Not authenticated, or an order is running.
            9 | 11 => self.state = State::Idle,
            // Authenticated and no order yet.
            10 => self.request(auth),
            // Answered.
            12 => {
                if kind != previous {
                    self.state = State::Answered;
                    self.read_key(auth);
                    self.stamp = now_ms;
                } else if self.refresh && self.elapsed_seconds(now_ms) >= REFRESH_SECONDS {
                    self.request(auth);
                }
            }
            // Timed out or refused.
            13 | 14 => {
                if kind != previous {
                    self.key.clear();
                    self.state = State::Failed;
                    self.stamp = now_ms;
                } else if self.elapsed_seconds(now_ms) >= REFRESH_SECONDS {
                    self.request(auth);
                }
            }
            _ => {}
        }
    }

    /// `0x1403B31C0`; its refusal (`6`, no order allowed right now) is not looked at.
    fn request(&mut self, auth: &mut Auth) {
        let _ = auth.request_download();
        self.state = State::Idle;
    }

    /// `0x14028FC60`: copies the order's URI into the key. An unavailable URI (`6`) empties the key; an empty
    /// or `null` one (`10`) is the empty key; a different text raises the "changed" byte.
    fn read_key(&mut self, auth: &Auth) {
        let text = match auth.download_uri() {
            Ok(text) => text,
            Err(10) => "",
            Err(_) => {
                self.key.clear();
                return;
            }
        };
        if self.key != text {
            self.key = text.to_owned();
            self.key_changed = true;
        }
    }
}
