//! The command-result record of the Thinca payment SDK (`spec/libam/payment.md` part "thinca" 3): libam's own
//! reading of the result objects the SDK hands its callbacks. The SDK boundary itself (the trait, the result
//! objects and the callback rings) is the backend interface [`backend::thinca`].

use crate::emoney::EMoneyDealInfo;
use crate::time::Tm;

use backend::thinca::{SdkDetail, SdkResult, kind};

/// The command-result record libam builds from a result (`0x140268780`, 220 bytes in the original).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRecord {
    /// `+0x00` result code.
    pub code: u32,
    /// `+0x04` Thinca brand number.
    pub sdk_brand: u32,
    /// `+0x08` text (deal number).
    pub text: String,
    /// `+0x4C` brand mask, bit n = Thinca brand n.
    pub brand_mask: u32,
    /// `+0x50` the deal.
    pub deal: EMoneyDealInfo,
    /// `+0xD8` a detail record was present, i.e. a deal exists.
    pub has_deal: bool,
    /// The DealInfo cash flag (`+132`).
    pub cash: bool,
}

impl ResultRecord {
    /// An empty record (code 0).
    pub fn empty() -> Self {
        Self {
            code: 0,
            sdk_brand: 0,
            text: String::new(),
            brand_mask: 0,
            deal: crate::emoney::deal_log::empty_deal(),
            has_deal: false,
            cash: false,
        }
    }

    /// The record builder `0x140268780`; `now` stamps the deal when the command kind asks for it.
    pub fn build(result: &SdkResult, now: Tm) -> Self {
        let mut record = Self::empty();
        record.code = result.code;
        record.sdk_brand = result.brand;
        record.text = result.text.chars().take(15).collect();
        // The text of the result is the deal number of the DealInfo (`0x140268780`).
        crate::emoney::deal_log::set_deal_number(&mut record.deal, &record.text);
        for &brand in result.brands.iter().filter(|&&brand| brand < 32) {
            record.brand_mask |= 1 << brand;
        }
        record.deal.status = match result.code {
            0 => 0,
            101 => 2,
            801 => 3,
            802 | 803 if !matches!(result.brand, 3 | 4 | 8) || result.brand == 7 => 3,
            _ => 1,
        };
        record.deal.brand = crate::emoney::deal_log::libam_brand_of_sdk(result.brand);
        if let Some(deal) = &result.deal {
            crate::emoney::deal_log::set_card(&mut record.deal, &deal.card);
            record.deal.balance_after = deal.balance_after;
        }
        if result.details.is_empty() {
            if matches!(result.kind, kind::PAYMENT | kind::BALANCE) {
                record.deal.time = now;
            }
        } else {
            for detail in &result.details {
                record.apply(detail);
            }
        }
        record
    }

    fn apply(&mut self, detail: &SdkDetail) {
        self.has_deal = true;
        let deal = &mut self.deal;
        match (detail.kind, detail.type_) {
            (1, 1) => {
                if let Some(time) = detail.time {
                    deal.time = Tm::from(time);
                }
                crate::emoney::deal_log::set_card(deal, &detail.card);
                deal.amount = detail.amount;
                if detail.has_balance == 1 {
                    deal.balance_before = detail.balance_before;
                    deal.balance_after = detail.balance_after;
                }
                if deal.status == 0 {
                    deal.status = 4;
                }
            }
            (1, 3) | (2, 3) if detail.kind == 1 || detail.status != 3 => {
                if let Some(time) = detail.time {
                    deal.time = Tm::from(time);
                }
                crate::emoney::deal_log::set_card(deal, &detail.card);
                deal.amount = detail.amount;
                if detail.has_balance == 1 {
                    deal.balance_before = detail.balance_before;
                }
                deal.balance_after = 0;
            }
            (1, 5) => {
                if let Some(time) = detail.time {
                    deal.time = Tm::from(time);
                }
                crate::emoney::deal_log::set_card(deal, &detail.card);
                deal.amount = 0;
                deal.balance_after = detail.balance_after;
                deal.balance_before = 0;
            }
            _ => {}
        }
    }
}
