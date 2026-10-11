//! The card scan state machine (`spec/libam/aime.md` E.3.7, E.4, `0x14034C440`) for MIFARE: Aime cards, Banapass and
//! amiibo. The FeliCa steps (SEQ 32..65) live in `felica.rs`.

use super::hub::{Hub, printable};
use super::wire::{Response, cmd};
use super::{CardKind, FelicaKind, PollStep, ScanResult};
use crate::aime::code::access_code_of_block;

/// SEQ numbers of the scan machine (the numbers of the original's `[328]`).
pub(super) mod seq {
    pub const WAIT_START: u8 = 0;
    pub const SETTLE: u8 = 1;
    pub const SEND_DETECT: u8 = 2;
    pub const WAIT_DETECT: u8 = 3;
    pub const SEND_DETECT_RETRY: u8 = 4;
    pub const SEND_SELECT: u8 = 5;
    pub const WAIT_SELECT: u8 = 6;
    pub const SEND_AUTH_B: u8 = 7;
    pub const WAIT_AUTH_B: u8 = 8;
    pub const SEND_AIME_BLOCK0: u8 = 9;
    pub const WAIT_AIME_BLOCK0: u8 = 10;
    pub const SEND_AIME_BLOCK1: u8 = 11;
    pub const WAIT_AIME_BLOCK1: u8 = 12;
    pub const SEND_AIME_BLOCK2: u8 = 13;
    pub const WAIT_AIME_BLOCK2: u8 = 14;
    pub const NEXT_CARD: u8 = 15;
    pub const SEND_AUTH_A: u8 = 16;
    pub const WAIT_AUTH_A: u8 = 17;
    pub const SEND_BANA_BLOCK0: u8 = 18;
    pub const WAIT_BANA_BLOCK0: u8 = 19;
    pub const SEND_BANA_BLOCK1: u8 = 20;
    pub const WAIT_BANA_BLOCK1: u8 = 21;
    pub const SEND_AMIIBO: u8 = 24;
    pub const WAIT_AMIIBO: u8 = 25;
    pub const REPOLL_WAIT: u8 = 26;
    pub const SEND_HALT: u8 = 27;
    pub const WAIT_HALT: u8 = 28;
    pub const SEND_STOP: u8 = 29;
    pub const WAIT_STOP: u8 = 30;
    pub const FELICA_LITE_CHECK: u8 = 32;
    pub const FELICA_LITE_CHECK_WAIT: u8 = 33;
    pub const FELICA_DFC: u8 = 34;
    pub const FELICA_DFC_WAIT: u8 = 35;
    pub const FELICA_RANDOM: u8 = 36;
    pub const FELICA_RANDOM_WAIT: u8 = 37;
    pub const FELICA_MAC: u8 = 38;
    pub const FELICA_MAC_WAIT: u8 = 39;
    pub const FELICA_LITE_READ: u8 = 40;
    pub const FELICA_LITE_READ_WAIT: u8 = 41;
    pub const FELICA_STD_CHECK: u8 = 42;
    pub const FELICA_STD_CHECK_WAIT: u8 = 43;
    pub const FELICA_STD_READ: u8 = 44;
    pub const FELICA_STD_READ_WAIT: u8 = 45;
    pub const FELICA_MOBILE_CHECK: u8 = 46;
    pub const FELICA_MOBILE_CHECK_WAIT: u8 = 47;
    pub const FELICA_MAIL: u8 = 54;
    pub const FELICA_MAIL_WAIT: u8 = 55;
    pub const FELICA_WEB: u8 = 56;
    pub const FELICA_WEB_WAIT: u8 = 57;
    pub const FELICA_MOBILE_OS: u8 = 58;
    pub const FELICA_MOBILE_OS_WAIT: u8 = 59;
    pub const FELICA_LITES_BLOCKS: u8 = 60;
    pub const FELICA_LITES_BLOCKS_WAIT: u8 = 61;
    pub const FELICA_LITES_BLOCK0: u8 = 64;
    pub const FELICA_LITES_BLOCK0_WAIT: u8 = 65;
    pub const FINISH: u8 = 31;
    pub const LAST: u8 = 66;
}

/// FeliCa class by PMm byte 1 (`table49` of the ctor).
fn felica_class(pmm1: u8) -> u8 {
    match pmm1 {
        1 | 8 | 9 | 13 | 32 | 50 => 2,
        6 | 7 => 3,
        16..=19 => 6,
        20..=31 => 7,
        240 => 1,
        241..=247 => 5,
        _ => 0,
    }
}

impl Hub {
    /// Clears the scan state and sends Start Polling (`0x1403317D0`).
    pub(super) fn start_polling(&mut self, u: usize, filter: u8, timeout_ms: u64, now: u64) {
        let timeout = self.units[u].config.command_timeout_ms;
        {
            let unit = &mut self.units[u];
            unit.filter = filter;
            unit.has_time_limit = timeout_ms != 0;
            unit.scan_deadline = now + timeout_ms;
            unit.led_cancelled = false;
            unit.stop = false;
            unit.felica_follow = 0;
            unit.card_count = 0;
            unit.cards = [super::hub::blank_card(), super::hub::blank_card()];
            unit.current = 0;
            unit.poll_again = false;
            unit.polling_started = false;
            unit.detect_retry = 0;
            unit.retry_counter = 0;
            unit.key_index = 0;
            unit.auth_kind = 0;
            unit.felica_follow = 0;
            unit.felica_class = 0;
            unit.felica_error = 0;
            unit.felica_retry = 0;
            unit.mobile_os = 0;
        }
        self.restart_polling(u, timeout, now);
    }

    /// Sends Start Polling again from SEQ 0.
    fn restart_polling(&mut self, u: usize, timeout: u32, now: u64) {
        let filter = self.units[u].filter;
        self.units[u].seq = seq::WAIT_START;
        self.units[u].start_time = now;
        self.send_armed(u, timeout, cmd::START_POLLING, &[filter]);
    }

    /// The scan result for the core.
    pub(super) fn scan_result(&self, u: usize) -> ScanResult {
        let unit = &self.units[u];
        let count = (unit.card_count as usize).min(2);
        ScanResult {
            card_count: unit.card_count,
            cards: unit.cards[..count].to_vec(),
            auth_kind: unit.auth_kind,
            felica_kind: match unit.felica_class {
                0 if unit.cards[0].kind != CardKind::FeliCa => None,
                1 => Some(FelicaKind::Lite),
                2 => Some(FelicaKind::Standard),
                5 => Some(FelicaKind::LiteS),
                class @ (3 | 6 | 7) => Some(FelicaKind::Mobile(class)),
                class => Some(FelicaKind::Other(class)),
            },
            felica_error: unit.felica_error,
            retry_counter: unit.retry_counter,
        }
    }

    /// Enters SEQ 29 (sets the post-stop deadline) or another SEQ.
    pub(super) fn goto(&mut self, u: usize, next: u8, now: u64) {
        let unit = &mut self.units[u];
        unit.seq = next;
        if next == seq::SEND_STOP {
            unit.post_stop_deadline = now + u64::from(unit.config.post_stop_delay_ms);
        }
    }

    /// One call of the scan machine.
    pub(super) fn poll_step(&mut self, u: usize, now: u64) -> PollStep {
        self.pump(now);
        if self.units[u].unit_error {
            self.units[u].rw = false;
            return PollStep::UnitError;
        }
        self.units[u].rw = true;
        if let Some(response) = self.units[u].response.take() {
            self.poll_response(u, &response, now);
        }
        let unit = &self.units[u];
        if (unit.pending || unit.forced_expiry) && unit.expired(now) && unit.seq <= seq::LAST {
            self.clear_unit(u);
            self.units[u].unit_error = true;
            return PollStep::UnitError;
        }
        self.poll_cancel(u, now);
        let timeout = self.units[u].config.command_timeout_ms;
        let current = self.units[u].current.min(1);
        let uid = self.units[u].cards[current].uid;
        let uid_block = |block: u8| [uid[0], uid[1], uid[2], uid[3], block];
        match self.units[u].seq {
            seq::SETTLE => {
                let unit = &mut self.units[u];
                if now > unit.start_time + u64::from(unit.config.detect_settle_ms) {
                    unit.seq = seq::SEND_DETECT;
                }
            }
            seq::SEND_DETECT | seq::SEND_DETECT_RETRY => {
                self.send_armed(u, timeout, cmd::CARD_DETECT, &[]);
                self.units[u].seq = seq::WAIT_DETECT;
            }
            seq::SEND_SELECT => {
                self.send_armed(u, timeout, cmd::SELECT_CARD, &uid);
                self.units[u].seq = seq::WAIT_SELECT;
            }
            seq::SEND_AUTH_B => {
                self.send_armed(u, timeout, cmd::AUTH_KEY_B, &uid_block(3));
                self.units[u].seq = seq::WAIT_AUTH_B;
            }
            step @ (seq::SEND_AIME_BLOCK0 | seq::SEND_AIME_BLOCK1 | seq::SEND_AIME_BLOCK2) => {
                let block = (step - seq::SEND_AIME_BLOCK0) / 2;
                self.send_armed(u, timeout, cmd::READ_BLOCK, &uid_block(block));
                self.units[u].seq = step + 1;
            }
            seq::SEND_AUTH_A => {
                self.send_armed(u, timeout, cmd::AUTH_KEY_A, &uid_block(3));
                self.units[u].seq = seq::WAIT_AUTH_A;
            }
            step @ (seq::SEND_BANA_BLOCK0 | seq::SEND_BANA_BLOCK1) => {
                let block = (step - seq::SEND_BANA_BLOCK0) / 2;
                self.send_armed(u, timeout, cmd::READ_BLOCK, &uid_block(block));
                self.units[u].seq = step + 1;
            }
            seq::SEND_AMIIBO => {
                self.send_armed(u, timeout, cmd::READ_BLOCK, &uid_block(21));
                self.units[u].seq = seq::WAIT_AMIIBO;
            }
            seq::NEXT_CARD => {
                let unit = &mut self.units[u];
                unit.current += 1;
                let done = unit.current >= unit.card_count as usize;
                self.goto(u, if done { seq::SEND_STOP } else { seq::SEND_SELECT }, now);
            }
            seq::SEND_HALT => {
                self.send_armed(u, timeout, cmd::HALT_CARD, &uid);
                self.units[u].seq = seq::WAIT_HALT;
            }
            seq::SEND_STOP => {
                if self.units[u].polling_started {
                    self.send_armed(u, timeout, cmd::STOP_POLLING, &[]);
                    self.units[u].seq = seq::WAIT_STOP;
                } else {
                    self.units[u].seq = seq::FINISH;
                }
            }
            seq::FINISH => return self.poll_finish(u, now),
            seq::REPOLL_WAIT => {
                if now > self.units[u].restart_at {
                    self.restart_polling(u, timeout, now);
                }
            }
            32..=65 => self.felica_step(u, now),
            _ => {}
        }
        PollStep::Running
    }

    /// SEQ 31: follow-up FeliCa flow, re-poll or finish.
    fn poll_finish(&mut self, u: usize, now: u64) -> PollStep {
        let unit = &mut self.units[u];
        if now <= unit.post_stop_deadline {
            return PollStep::Running;
        }
        let next = match unit.felica_follow {
            1 => 32,
            6 => 42,
            8 => 46,
            12 => 54,
            13 => 56,
            14 => 58,
            _ => 0,
        };
        if next != 0 {
            unit.felica_follow = 0;
            unit.seq = next;
            return PollStep::Running;
        }
        if unit.poll_again {
            unit.poll_again = false;
            unit.seq = seq::REPOLL_WAIT;
            unit.restart_at = now + u64::from(unit.config.repoll_wait_ms);
            return PollStep::Running;
        }
        unit.rw = false;
        if std::mem::take(&mut unit.stop) { PollStep::Stopped } else { PollStep::Finished }
    }

    /// Cancel requests move the machine toward the stop (`[310]`).
    fn poll_cancel(&mut self, u: usize, now: u64) {
        let unit = &self.units[u];
        if !unit.stop {
            return;
        }
        match unit.seq {
            5 | 7 | 9 | 18 | 11 | 20 | 13 | 22 | 27 | 15 => self.goto(u, seq::SEND_STOP, now),
            2 | 4 | 1 | 26 | 32 | 34 | 36 | 38 | 60 | 64 | 40 | 42 | 44 | 46 | 48 | 50 | 52 | 54 | 56 => {
                let unit = &mut self.units[u];
                unit.seq = seq::FINISH;
                unit.felica_follow = 0;
            }
            seq::FINISH if unit.poll_again => {
                let unit = &mut self.units[u];
                unit.felica_follow = 0;
                unit.poll_again = false;
            }
            _ => {}
        }
    }

    /// Handles a response to the command of the current wait state (the original's `decodeCommand` dispatch).
    fn poll_response(&mut self, u: usize, response: &Response, now: u64) {
        match response.cmd {
            cmd::START_POLLING => {
                self.units[u].polling_started = true;
                self.units[u].seq = seq::SETTLE;
            }
            cmd::STOP_POLLING => {
                if response.status == 0 {
                    self.units[u].polling_started = false;
                    self.units[u].seq = seq::FINISH;
                }
            }
            cmd::CARD_DETECT => self.on_detect(u, response, now),
            cmd::SELECT_CARD => {
                if response.status != 0 {
                    if !self.card_retry(u, true) {
                        self.goto(u, seq::SEND_HALT, now);
                    }
                    return;
                }
                let unit = &mut self.units[u];
                if unit.amiibo_mode {
                    unit.key_index = 2;
                }
                unit.seq = match unit.key_index {
                    0 => seq::SEND_AUTH_B,
                    1 => seq::SEND_AUTH_A,
                    _ => seq::SEND_AMIIBO,
                };
            }
            cmd::HALT_CARD => self.goto(u, seq::NEXT_CARD, now),
            cmd::FELICA_THROUGH | cmd::MOBILE_OPEN => self.felica_response(u, response, now),
            cmd::AUTH_KEY_B => {
                let unit = &mut self.units[u];
                if response.status == 0 {
                    unit.auth_kind = 1;
                    unit.seq = seq::SEND_AIME_BLOCK0;
                } else {
                    unit.key_index = 1;
                    unit.seq = seq::SEND_DETECT;
                }
            }
            cmd::AUTH_KEY_A => {
                if response.status == 0 {
                    self.units[u].auth_kind = 2;
                    self.units[u].seq = seq::SEND_BANA_BLOCK0;
                } else if !self.card_retry(u, false) {
                    self.goto(u, seq::SEND_HALT, now);
                }
            }
            cmd::READ_BLOCK => self.on_read(u, response, now),
            _ => {}
        }
    }

    /// The card retry rule of the dispatcher (`[41905] < [24]`): restart the sequence. `select` restarts at the
    /// detect; the read family restarts at the detect for generation 1 readers and at the key B auth otherwise.
    fn card_retry(&mut self, u: usize, select: bool) -> bool {
        let unit = &mut self.units[u];
        if unit.retry_counter >= unit.config.card_retries {
            return false;
        }
        unit.retry_counter += 1;
        unit.key_index = 0;
        unit.seq = if select || unit.info.generation <= 1 { seq::SEND_DETECT } else { seq::SEND_AUTH_B };
        true
    }

    /// The Card Detect response (`0x140349D30`).
    fn on_detect(&mut self, u: usize, response: &Response, now: u64) {
        let unit = &mut self.units[u];
        let may_repoll = !unit.has_time_limit || now < unit.scan_deadline;
        if response.status != 0 {
            unit.poll_again |= may_repoll;
            self.goto(u, seq::SEND_STOP, now);
            return;
        }
        let payload = &response.payload;
        let count = payload.first().copied().unwrap_or(0);
        unit.card_count = u32::from(count);
        self.goto(u, seq::SEND_STOP, now);
        let unit = &mut self.units[u];
        if count == 0 {
            unit.poll_again |= may_repoll;
            return;
        }
        if count > 1 {
            return;
        }
        let kind = payload.get(1).copied().unwrap_or(0);
        let len = usize::from(payload.get(2).copied().unwrap_or(0));
        let id = payload.get(3..).unwrap_or(&[]);
        let id = &id[..len.min(id.len())];
        let amiibo = unit.amiibo_mode;
        let card = &mut unit.cards[0];
        card.detect_len = len as u32;
        match (kind, len) {
            (0x20, _) => {
                let n = id.len().min(16);
                card.idm_pmm[..n].copy_from_slice(&id[..n]);
                card.kind = CardKind::FeliCa;
                card.valid = true;
            }
            (0x10, 7) if amiibo => {
                card.kind = CardKind::Amiibo;
                card.uid.copy_from_slice(&id[..4]);
                card.uid_copy[..7].copy_from_slice(id);
            }
            (0x10, _) => {
                card.kind = CardKind::Mifare;
                let n = id.len().min(4);
                card.uid[..n].copy_from_slice(&id[..n]);
            }
            _ => {
                if unit.detect_retry < 3 {
                    unit.detect_retry += 1;
                    unit.poll_again = true;
                } else {
                    card.kind = CardKind::DetectFailure;
                }
                return;
            }
        }
        unit.current = 0;
        match unit.cards[0].kind {
            CardKind::Mifare | CardKind::Amiibo => unit.seq = seq::SEND_SELECT,
            CardKind::FeliCa => self.classify_felica(u),
            _ => {}
        }
    }

    /// FeliCa classification (E.4.2): chip class and the flow to run once polling stopped.
    fn classify_felica(&mut self, u: usize) {
        let unit = &mut self.units[u];
        let class = felica_class(unit.cards[0].idm_pmm[9]);
        unit.felica_class = class;
        unit.felica_follow = match class {
            1 | 5 => 1,
            2 => 6,
            3 | 6 | 7 => match unit.follow_up {
                Some(super::FollowUp::Mail(_)) => 12,
                Some(super::FollowUp::Web(_)) => 13,
                None => 8,
            },
            _ => 0,
        };
        unit.mobile_os = match class {
            3 => 1,
            6 => 2,
            7 => 3,
            _ => 0,
        };
    }

    /// A MIFARE Read Block response, by the wait state it answers.
    fn on_read(&mut self, u: usize, response: &Response, now: u64) {
        let block: Option<[u8; 16]> = response.payload.get(..16).and_then(|bytes| bytes.try_into().ok());
        let Some(block) = block.filter(|_| response.status == 0) else {
            if !self.card_retry(u, false) {
                self.goto(u, seq::SEND_HALT, now);
            }
            return;
        };
        let unit = &mut self.units[u];
        let step = unit.seq;
        let current = unit.current.min(1);
        match step {
            seq::WAIT_AIME_BLOCK0 | seq::WAIT_BANA_BLOCK0 => {
                unit.cards[current].block0 = block;
                unit.seq = step + 1;
            }
            seq::WAIT_AIME_BLOCK1 => {
                unit.block1 = block;
                let card = &mut unit.cards[current];
                card.name = printable(&block[..4]);
                card.value = u32::from_be_bytes([block[12], block[13], block[14], block[15]]);
                unit.seq = seq::SEND_AIME_BLOCK2;
            }
            seq::WAIT_AIME_BLOCK2 => {
                let mut code = [0u8; 10];
                code.copy_from_slice(&block[6..]);
                let accepted = if code[0] >> 4 == 3 {
                    // A Banapass in disguise: the code comes from block 1.
                    access_code_of_block(&unit.block1)
                } else if code[..5].iter().all(|&byte| byte == 0) {
                    if unit.retry_counter < unit.config.card_retries {
                        unit.retry_counter += 1;
                        unit.seq = seq::SEND_AIME_BLOCK2;
                        return;
                    }
                    None
                } else {
                    Some(code)
                };
                match accepted {
                    Some(code) => {
                        let card = &mut unit.cards[current];
                        card.access_code = code;
                        card.valid = true;
                        unit.seq = seq::NEXT_CARD;
                    }
                    None => self.goto(u, seq::SEND_HALT, now),
                }
            }
            seq::WAIT_BANA_BLOCK1 => match access_code_of_block(&block) {
                Some(code) => {
                    let card = &mut unit.cards[current];
                    card.access_code = code;
                    card.valid = true;
                    unit.seq = seq::NEXT_CARD;
                }
                None => self.goto(u, seq::SEND_HALT, now),
            },
            seq::WAIT_AMIIBO => {
                let card = &mut unit.cards[current];
                card.amiibo.head = u32::from_be_bytes([0, block[0], block[1], block[2]]);
                card.amiibo.byte3 = block[3];
                card.amiibo.word = u16::from_be_bytes([block[4], block[5]]);
                card.amiibo.byte6 = block[6];
                card.valid = true;
                unit.seq = seq::NEXT_CARD;
            }
            _ => {}
        }
    }
}
