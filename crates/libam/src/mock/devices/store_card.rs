//! Store authentication cards for the simulated Aime reader: the issuer side of the card format that the
//! e-money module reads (`spec/libam/payment.md` PayEmSvc section 6).

use aes::Aes256;
use cbc::Encryptor;
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::emoney::card::KEY_TABLE;
use crate::emoney::{CardRead, StoreCard};

/// Builds the blocks of a card (the inverse of the e-money module's `read_store_card`).
pub fn encrypt_card(uid: &[u8], kind: u8, card: &StoreCard) -> CardRead {
    let uid4 = &uid[uid.len() - 4..];
    let message: Vec<u8> = uid4.iter().copied().cycle().take(32).collect();
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&KEY_TABLE).expect("any key length");
    mac.update(&message);
    let digest = mac.finalize().into_bytes();
    let mut iv = [0u8; 16];
    for (index, byte) in iv.iter_mut().enumerate() {
        *byte = digest[index] ^ digest[index + 16];
    }
    let mut plain = [0u8; 144];
    for (text, start, len) in
        [(&card.store_card_id, 0, 16), (&card.merchant_code, 16, 20), (&card.branch, 36, 5), (&card.aux, 48, 16)]
    {
        let bytes = text.as_bytes();
        plain[start..start + bytes.len().min(len)].copy_from_slice(&bytes[..bytes.len().min(len)]);
    }
    let mut cipher = plain;
    Encryptor::<Aes256>::new_from_slices(&digest, &iv)
        .expect("32-byte key and 16-byte iv")
        .encrypt_padded_mut::<NoPadding>(&mut cipher, 144)
        .expect("144 is a multiple of 16");
    let mut blocks = vec![[b'T', b'C', kind, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]];
    for chunk in cipher[..128].chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        blocks.push(block);
    }
    CardRead { uid: uid.to_vec(), blocks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emoney::card::{CardRefusal, read_store_card};

    #[test]
    fn a_card_round_trips_and_a_wrong_type_is_refused() {
        let card = StoreCard {
            store_card_id: "0123456789ABCDEF".into(),
            merchant_code: "M0000000000000000001".into(),
            branch: "00123".into(),
            aux: "AUXAUXAUXAUXAUXX".into(),
        };
        let read = encrypt_card(&[1, 2, 3, 4, 5, 6, 7], 2, &card);
        assert_eq!(read_store_card(&read, 2), Ok(card));
        assert_eq!(read_store_card(&read, 3), Err(CardRefusal::Type));
    }
}
