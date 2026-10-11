//! The `auth_data` verification (`spec/libam/allnet.md` part 6 section 5): a DER CMS SignedData whose signer
//! certificate chains to the trust store, found among the certificates the TLS server presented or the CMS
//! carries. Pure RustCrypto (`cms`, `x509-cert`, `rsa`, `sha2`); RSA PKCS#1 v1.5 with SHA-256 is the supported
//! signature suite.

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier};
use der::asn1::{ObjectIdentifier, OctetString};
use der::{Decode, Encode};
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use sha2::{Digest, Sha256};
use x509_cert::Certificate;

const SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const SHA256_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
const RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");

/// Unix time (UTC seconds) the validity dates are compared with.
pub type UnixSeconds = u64;

/// The longest signed content alAuth accepts (`CMS_get0_content` cap).
pub const CONTENT_CAP: usize = 256;

fn rsa_verify(key_cert: &Certificate, message: &[u8], signature: &[u8]) -> bool {
    let Ok(spki) = key_cert.tbs_certificate.subject_public_key_info.to_der() else { return false };
    let Ok(key) = RsaPublicKey::from_public_key_der(&spki) else { return false };
    let Ok(signature) = Signature::try_from(signature) else { return false };
    VerifyingKey::<Sha256>::new(key).verify(message, &signature).is_ok()
}

fn valid_at(cert: &Certificate, now: UnixSeconds) -> bool {
    let validity = &cert.tbs_certificate.validity;
    validity.not_before.to_unix_duration().as_secs() <= now && now <= validity.not_after.to_unix_duration().as_secs()
}

/// Whether `cert` chains to a trusted anchor through `pool`. `anchors` are DER certificates; `None` means the
/// transport already validated the TLS chain, so the certificates of the chain itself are the anchors.
fn chains(cert: &Certificate, pool: &[Certificate], anchors: &[Certificate], now: UnixSeconds, depth: usize) -> bool {
    if depth > 8 || !valid_at(cert, now) {
        return false;
    }
    let der = cert.to_der().unwrap_or_default();
    if anchors.iter().any(|a| a.to_der().is_ok_and(|d| d == der)) {
        return true;
    }
    let candidates =
        anchors.iter().chain(pool.iter()).filter(|c| c.tbs_certificate.subject == cert.tbs_certificate.issuer);
    for issuer in candidates {
        if cert.signature_algorithm.oid != SHA256_WITH_RSA {
            continue;
        }
        let (Ok(tbs), Some(sig)) = (cert.tbs_certificate.to_der(), cert.signature.as_bytes()) else { continue };
        if !rsa_verify(issuer, &tbs, sig) {
            continue;
        }
        let self_signed = issuer.tbs_certificate.subject == issuer.tbs_certificate.issuer;
        let issuer_der = issuer.to_der().unwrap_or_default();
        if anchors.iter().any(|a| a.to_der().is_ok_and(|d| d == issuer_der)) && valid_at(issuer, now) {
            return true;
        }
        if !self_signed && chains(issuer, pool, anchors, now, depth + 1) {
            return true;
        }
    }
    false
}

/// `0x1403FC230`: verifies `der` and returns the signed content (at most [`CONTENT_CAP`] bytes).
pub fn verify_auth_data(
    der: &[u8],
    peer_chain: &[Vec<u8>],
    anchors: Option<&[Vec<u8>]>,
    now: UnixSeconds,
) -> Option<Vec<u8>> {
    if peer_chain.is_empty() {
        return None;
    }
    let info = ContentInfo::from_der(der).ok()?;
    let signed: SignedData = info.content.decode_as().ok()?;
    let mut pool: Vec<Certificate> = peer_chain.iter().filter_map(|c| Certificate::from_der(c).ok()).collect();
    let peer_count = pool.len();
    if let Some(embedded) = &signed.certificates {
        for choice in embedded.0.iter() {
            if let CertificateChoices::Certificate(cert) = choice {
                pool.push(cert.clone());
            }
        }
    }
    let anchor_certs: Vec<Certificate> = match anchors {
        Some(list) => list.iter().filter_map(|c| Certificate::from_der(c).ok()).collect(),
        None => pool[..peer_count].to_vec(),
    };
    let content = signed.encap_content_info.econtent.as_ref()?.decode_as::<OctetString>().ok()?;
    let content = content.as_bytes();
    if content.len() > CONTENT_CAP {
        return None;
    }
    let mut any_chain = false;
    for signer in signed.signer_infos.0.iter() {
        let SignerIdentifier::IssuerAndSerialNumber(id) = &signer.sid else { continue };
        let Some(cert) = pool
            .iter()
            .find(|c| c.tbs_certificate.issuer == id.issuer && c.tbs_certificate.serial_number == id.serial_number)
        else {
            continue;
        };
        if signer.digest_alg.oid != SHA256
            || !(signer.signature_algorithm.oid == RSA || signer.signature_algorithm.oid == SHA256_WITH_RSA)
        {
            continue;
        }
        let digest = Sha256::digest(content);
        let signed_bytes = match &signer.signed_attrs {
            Some(attrs) => {
                let matches = attrs.iter().any(|a| {
                    a.oid == MESSAGE_DIGEST
                        && a.values
                            .iter()
                            .any(|v| v.decode_as::<OctetString>().is_ok_and(|o| o.as_bytes() == &digest[..]))
                });
                if !matches {
                    continue;
                }
                match attrs.to_der() {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                }
            }
            None => content.to_vec(),
        };
        if !rsa_verify(cert, &signed_bytes, signer.signature.as_bytes()) {
            continue;
        }
        if chains(cert, &pool, &anchor_certs, now, 0) {
            any_chain = true;
            break;
        }
    }
    any_chain.then(|| content.to_vec())
}
