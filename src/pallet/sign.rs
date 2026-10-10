//! Pallet signatures (#378, stormuefi#18).
//!
//! One Ed25519 signature over [`signing_message`](stormblock_pallet_format::signing_message):
//! the manifest digest (so the whole member set), the pallet's version and its
//! kind. It sits in the superblock with the key's id (`layout::sb::SIG_*`),
//! where firmware finds it without a filesystem. stormuefi verifies it with the
//! keys compiled into it (stormuefi#59).
//!
//! **Signing is two steps, so the key never leaves the signer** (stormcentral,
//! after its gates, stormcentral#634): read a built pallet's message, sign it
//! wherever the key lives, attach the signature. Attaching checks the
//! signature against the public key given before it writes anything, and
//! rewrites only the superblock (the signature bytes and its CRC): the pallet
//! is not rebuilt, and its manifest digest, members and content are untouched.

use stormblock_pallet_format::{self as fmt1, layout, Signature, SignatureFault, KEY_ID_LEN, SIGNATURE_LEN};

/// The key id of an Ed25519 public key.
pub fn key_id(public_key: &[u8; 32]) -> [u8; KEY_ID_LEN] {
    fmt1::key_id_of(public_key)
}

/// One line for a pallet's signature: `unsigned`, `ed25519:<key id>`, or
/// `malformed: …`.
pub fn describe(sig: Signature) -> String {
    match sig {
        Signature::Unsigned => "unsigned".into(),
        Signature::Ed25519 { key_id, .. } => format!("ed25519:{}", hex::encode(key_id)),
        Signature::Malformed(SignatureFault::UnknownAlgorithm(a)) => format!("malformed: unknown algorithm {a}"),
        Signature::Malformed(SignatureFault::StrayBytes) => "malformed: signature bytes with no algorithm".into(),
        Signature::Malformed(SignatureFault::EmptySignature) => "malformed: an Ed25519 signature of zeros".into(),
    }
}

/// Whether `signature` is `public_key`'s Ed25519 signature over `message`.
pub fn verifies(public_key: &[u8; 32], message: &[u8], signature: &[u8; SIGNATURE_LEN]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(message, signature)
        .is_ok()
}

/// Sign `message` with an Ed25519 private key's 32-byte seed: the public key
/// and the signature. For a signer that keeps the seed in a file; one that
/// keeps it elsewhere signs the message itself and attaches the result.
pub fn sign(seed: &[u8; 32], message: &[u8]) -> Result<([u8; 32], [u8; SIGNATURE_LEN]), String> {
    use ring::signature::KeyPair;
    let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).map_err(|e| format!("not an Ed25519 seed: {e}"))?;
    let mut pk = [0u8; 32];
    pk.copy_from_slice(kp.public_key().as_ref());
    let mut sig = [0u8; SIGNATURE_LEN];
    sig.copy_from_slice(kp.sign(message).as_ref());
    Ok((pk, sig))
}

/// Write the signature into a superblock's bytes (the first
/// `SUPERBLOCK_LEN` of a pallet) and seal its CRC again. Checks first: the
/// signature must be `public_key`'s over this superblock's message, or
/// nothing is changed.
pub fn attach(header: &mut [u8], public_key: &[u8; 32], signature: &[u8; SIGNATURE_LEN]) -> Result<(), String> {
    use layout::sb as o;
    let sb = fmt1::Superblock::parse(header).map_err(|e| format!("not a pallet superblock: {e:?}"))?;
    let message = sb.signing_message();
    if !verifies(public_key, &message, signature) {
        return Err(format!(
            "the signature is not key {}'s over this pallet's message (manifest {}, version {}); nothing was written",
            hex::encode(key_id(public_key)),
            hex::encode(sb.manifest_digest),
            sb.pallet_version
        ));
    }
    header[o::SIG_ALG..o::SIG_ALG + 4].copy_from_slice(&fmt1::SIG_ALG_ED25519.to_le_bytes());
    header[o::SIG_KEY_ID..o::SIG_KEY_ID + KEY_ID_LEN].copy_from_slice(&key_id(public_key));
    header[o::SIGNATURE..o::SIGNATURE + SIGNATURE_LEN].copy_from_slice(signature);
    let crc = fmt1::superblock_crc(header);
    header[o::SUPERBLOCK_CRC..o::SUPERBLOCK_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// What a superblock says about signing it: its state, and the message.
pub fn report(sb: &fmt1::Superblock) -> serde_json::Value {
    let sig = sb.signature();
    serde_json::json!({
        "signature": describe(sig),
        "key_id": match sig { Signature::Ed25519 { key_id, .. } => Some(hex::encode(key_id)), _ => None },
        "message": hex::encode(sb.signing_message()),
        "manifest_digest": hex::encode(sb.manifest_digest),
        "pallet_version": sb.pallet_version,
    })
}

/// 32 bytes from hex (a public key, a seed).
pub fn hex32(s: &str) -> Result<[u8; 32], String> {
    let b = hex::decode(s.trim()).map_err(|e| format!("not hex: {e}"))?;
    b.try_into().map_err(|b: Vec<u8>| format!("{} bytes, not 32", b.len()))
}

/// 64 bytes from hex (a signature).
pub fn hex64(s: &str) -> Result<[u8; SIGNATURE_LEN], String> {
    let b = hex::decode(s.trim()).map_err(|e| format!("not hex: {e}"))?;
    b.try_into().map_err(|b: Vec<u8>| format!("{} bytes, not 64", b.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_and_a_wrong_key_or_message_fails() {
        let (pk, sig) = sign(&[7u8; 32], b"hello").unwrap();
        assert!(verifies(&pk, b"hello", &sig));
        assert!(!verifies(&pk, b"hellO", &sig));
        let (other, _) = sign(&[8u8; 32], b"x").unwrap();
        assert!(!verifies(&other, b"hello", &sig));
        assert_eq!(key_id(&pk).len(), 16);
    }
}
