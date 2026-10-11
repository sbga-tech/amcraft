//! Value types of the Aime Pay module: the location, the deal record and its ring, the deal summary, the item
//! list and the user setting (`payment.md` PayApMgr 2.3-2.7).

use std::collections::VecDeque;

use libam_api::aime::AccessCode;
use libam_api::aime_pay::{AimePayDeal, AimePayLocation, AimePaySummaryItem, AimePayUserSetting};
use libam_api::time::Tm;

use crate::util::wide_array;

/// `amount` of a deal whose price is not known yet.
pub const NO_AMOUNT: u32 = 0xFFFF_FFFF;
/// Capacity of the deal log ring (`payment.apmgr.rec.deallog_ring`).
pub const DEAL_RING: usize = 5;
/// Capacity of the deal summary (`payment.apmgr.rec.summary`).
pub const SUMMARY_RING: usize = 7;
/// Capacity of the item list (`payment.apmgr.rec.itemlist`).
pub const ITEM_RING: usize = 20;

/// Longest store / company code (`wchar[11]`).
pub const CODE_LEN: usize = 10;
/// Longest store name (`wchar[21]`).
pub const STORE_NAME_LEN: usize = 20;
/// Longest company name (`wchar[41]`).
pub const COMPANY_NAME_LEN: usize = 40;

/// `am::aimepay::Location` (`payment.apmgr.rec.location`): four strings, all non-empty for a valid location.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    /// Store code, at most [`CODE_LEN`] characters.
    pub store_code: String,
    /// Store name, at most [`STORE_NAME_LEN`] characters.
    pub store_name: String,
    /// Company code, at most [`CODE_LEN`] characters.
    pub company_code: String,
    /// Company name, at most [`COMPANY_NAME_LEN`] characters.
    pub company_name: String,
}

/// Number of UTF-16 units of `text` (the unit the original's `wcsnlen` limits count).
pub fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

impl Location {
    /// `0x1402226A0`: every string is non-empty.
    pub fn is_complete(&self) -> bool {
        !(self.store_code.is_empty()
            || self.store_name.is_empty()
            || self.company_code.is_empty()
            || self.company_name.is_empty())
    }

    /// `0x14023B490`: the same shop when store code and company code are equal.
    pub fn same_shop(&self, other: &Self) -> bool {
        self.store_code == other.store_code && self.company_code == other.company_code
    }

    /// The location as the controller sees it (`convertLoc`, `0x14001AF60`).
    pub fn to_api(&self) -> AimePayLocation {
        AimePayLocation {
            valid: self.is_complete(),
            name: (!self.store_name.is_empty()).then(|| wide_array::<21>(&self.store_name)),
            company_name: (!self.company_name.is_empty()).then(|| wide_array::<41>(&self.company_name)),
        }
    }
}

/// `am::aimepay::DealLog` (`payment.apmgr.rec.deallog`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deal {
    /// 0 ok, 1 failed, 2 cancelled, 3 unknown.
    pub status: u32,
    /// Timestamp; `time.year != 0` means "set".
    pub time: Tm,
    /// Server `result_code`.
    pub error_code: u32,
    /// Server `detail_code`, at most 15 characters.
    pub error_message: String,
    /// Item count.
    pub item_count: u32,
    /// Item id (13 digits).
    pub item_id: String,
    /// Packed-BCD access code.
    pub access_code: AccessCode,
    /// Receipt id.
    pub receipt_id: u64,
    /// Amount, [`NO_AMOUNT`] while unknown.
    pub amount: u32,
    /// Coin payment.
    pub is_coin: bool,
}

impl Default for Deal {
    fn default() -> Self {
        Self {
            status: 0,
            time: Tm::default(),
            error_code: 0,
            error_message: String::new(),
            item_count: 0,
            item_id: String::new(),
            access_code: AccessCode::UNSET,
            receipt_id: 0,
            amount: NO_AMOUNT,
            is_coin: false,
        }
    }
}

impl Deal {
    /// The deal has a timestamp (`tm_year != 0`).
    pub fn has_time(&self) -> bool {
        self.time.year != 0
    }

    /// The deal as the controller sees it.
    pub fn to_api(&self) -> AimePayDeal {
        AimePayDeal {
            status: self.status,
            time: self.time,
            error_code: self.error_code,
            error_message: wide_array::<16>(&self.error_message),
            item_count: self.item_count,
            item_id: wide_array::<14>(&self.item_id),
            access_code: self.access_code.0,
            receipt_id: self.receipt_id,
            amount: self.amount,
            is_coin: self.is_coin,
        }
    }

    /// The `errorMessage` the server answer fills in: the first 15 characters of `detail_code`.
    pub fn set_error_message(&mut self, detail: &str) {
        self.error_message = detail.chars().take(15).collect();
    }
}

/// The deal log: newest first, at most [`DEAL_RING`] entries (`0x140221E00`).
#[derive(Debug, Clone, Default)]
pub struct DealRing(VecDeque<Deal>);

impl DealRing {
    /// Appends `deal` as the newest entry; the oldest one drops out when the ring is full.
    pub fn push(&mut self, deal: Deal) {
        self.0.push_front(deal);
        self.0.truncate(DEAL_RING);
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Entry `index`, newest first.
    pub fn get(&self, index: usize) -> Option<&Deal> {
        self.0.get(index)
    }

    /// Empties the ring.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// One entry of the deal summary (`payment.apmgr.rec.summary`), 52 bytes in the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// `target_date`.
    pub date: Tm,
    /// `amount_count`.
    pub amount_count: u32,
    /// `amount_price`.
    pub amount_price: u32,
    /// `settlement_count`.
    pub settlement_count: u32,
    /// `settlement_price`.
    pub settlement_price: u32,
}

impl Summary {
    /// What the controller publishes: `count = amount_count`, `amount = amount_price`.
    pub fn to_api(self) -> AimePaySummaryItem {
        AimePaySummaryItem { date: self.date, count: self.amount_count, amount: self.amount_price }
    }
}

/// One entry of the item list (`payment.apmgr.rec.itemlist`). Only the price is used by libam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 13 digits.
    pub item_id: String,
    /// Tax-included price.
    pub price: u32,
}

/// `UserSetting` (`payment.apmgr.rec.usersetting`): valid once a CheckUser answer filled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSetting {
    /// The id the answer belongs to.
    pub aime_id: u32,
    /// `credit_flg`.
    pub credit_card_available: bool,
    /// `touchskip_flg`.
    pub aime_hold_skip: bool,
}

impl UserSetting {
    /// The controller's view.
    pub fn to_api(self) -> AimePayUserSetting {
        AimePayUserSetting {
            aime_id: self.aime_id,
            credit_card_available: self.credit_card_available,
            aime_hold_skip: self.aime_hold_skip,
        }
    }
}

/// 13 decimal digits exactly (`0x14023B950`).
pub fn is_item_id(text: &str) -> bool {
    text.len() == 13 && text.bytes().all(|byte| byte.is_ascii_digit())
}
