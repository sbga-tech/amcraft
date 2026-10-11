//! What the e-money flows need from the credit and sequence modules (`spec/libam/payment.md` PayEmMgr `coinPay`,
//! PayEmCmd task 5): the credit gate `0x1402536E0` in front of `coinPay` and `credit.addCoin` (`0x14022BAC0`) at the
//! end of a paid coin deal.

use crate::result::{AmError, AmResult};

use crate::credit::Credit;

/// The credit module as the payment flow sees it.
pub trait CreditLink {
    /// `credit.addCoin(player, coins)` (`0x14022BAC0`): `true` when the coins went into the credit.
    fn add_coin(&mut self, player: u32, coins: u32) -> bool;
}

impl CreditLink for Credit {
    fn add_coin(&mut self, player: u32, coins: u32) -> bool {
        Credit::add_coin(self, player, coins)
    }
}

/// The credit link of a caller that has no credit module (a refused add).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCredit;

impl CreditLink for NoCredit {
    fn add_coin(&mut self, _player: u32, _coins: u32) -> bool {
        false
    }
}

/// The inputs of the credit gate `0x1402536E0`, sampled from the sequence and credit modules for one `coinPay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinGate {
    /// The sequence manager is in state 2 or 5 (test mode).
    pub test_mode: bool,
    /// The credit module's "ignore coin" byte (`credit+1212`).
    pub ignore_coin: bool,
    /// Coins still addable for the player's credit unit (`0x14022BDD0`).
    pub addable: u32,
    /// `coinChuteMultiplier[0]` of the credit rule block.
    pub multiplier: u32,
    /// The coin cost of the credit rule block (`coinAmount`, `0x14021D350`): the price of one coin.
    pub coin_amount: u32,
}

impl CoinGate {
    /// The gate: result 2 in test mode, with coins ignored and without addable coins; result 1 when `coin` coins
    /// would exceed the addable ones.
    pub fn check(&self, coin: u32) -> AmResult<()> {
        if self.test_mode || self.ignore_coin || self.addable == 0 {
            return Err(AmError::code(AmError::NOT_INITIALIZED));
        }
        if u64::from(self.addable) < u64::from(coin) * u64::from(self.multiplier) {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> CoinGate {
        CoinGate { test_mode: false, ignore_coin: false, addable: 10, multiplier: 2, coin_amount: 100 }
    }

    #[test]
    fn gate_results() {
        assert!(gate().check(5).is_ok());
        assert_eq!(gate().check(6).unwrap_err().code, AmError::INVALID_ARGUMENT);
        assert_eq!(CoinGate { test_mode: true, ..gate() }.check(1).unwrap_err().code, AmError::NOT_INITIALIZED);
        assert_eq!(CoinGate { ignore_coin: true, ..gate() }.check(1).unwrap_err().code, AmError::NOT_INITIALIZED);
        assert_eq!(CoinGate { addable: 0, ..gate() }.check(1).unwrap_err().code, AmError::NOT_INITIALIZED);
    }
}
