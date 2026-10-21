//! NVMe in-band authentication: DH-HMAC-CHAP (NVMe Base 2.0 §8.13, TP 8006).
//!
//! What this implements, and why no more:
//! - **Unidirectional, NULL DH group.** The host proves it holds the secret
//!   the engine minted for its NQN; that is what stops a host that merely
//!   *says* another host's NQN (#210). A DH group adds a session key, which
//!   only secure-channel concatenation (TLS) consumes, and there is no TLS
//!   on this transport. Linux offers the NULL group whenever it is not asked
//!   for concatenation.
//! - **SHA-256/384/512**, the three hashes the spec defines; the host's
//!   first usable one is taken (Linux lists SHA-256 first).
//! - **DHHC-1 secrets**, the format `nvme gen-dhchap-key` writes and
//!   `nvme connect --dhchap-secret` reads: `DHHC-1:<hh>:<base64(key ‖ crc32)>:`
//!   where `hh` names the hash that transforms the key with the host NQN.
//!
//! Bidirectional authentication (a controller secret) is refused the way
//! Linux's own target refuses it without a controller key: Success1 is
//! replaced by Failure1.
//!
//! The wire layout and every byte fed to the HMAC are taken from Linux's
//! `drivers/nvme/{common,host,target}/auth.c` and `include/linux/nvme.h`,
//! because the kernel is the initiator that has to agree with us. HMAC is
//! written here over `sha2` rather than pulled in as a crate; RFC 4231's
//! vectors pin it.

use sha2::Digest;

/// `SECP` of an Authentication Send/Receive carrying DH-HMAC-CHAP.
pub const SECP_DHCHAP: u8 = 0xE9;
/// Fabrics command types.
pub const FCTYPE_AUTH_SEND: u8 = 0x05;
pub const FCTYPE_AUTH_RECEIVE: u8 = 0x06;

/// `auth_type` of a message.
pub const AUTH_COMMON: u8 = 0x00;
pub const AUTH_DHCHAP: u8 = 0x01;

/// `auth_id` of a message.
pub const MSG_NEGOTIATE: u8 = 0x00;
pub const MSG_CHALLENGE: u8 = 0x01;
pub const MSG_REPLY: u8 = 0x02;
pub const MSG_SUCCESS1: u8 = 0x03;
pub const MSG_SUCCESS2: u8 = 0x04;
pub const MSG_FAILURE2: u8 = 0xF0;
pub const MSG_FAILURE1: u8 = 0xF1;

/// DH-HMAC-CHAP's protocol identifier in a negotiate descriptor.
pub const DHCHAP_AUTH_ID: u8 = 0x01;

pub const HASH_SHA256: u8 = 0x01;
pub const HASH_SHA384: u8 = 0x02;
pub const HASH_SHA512: u8 = 0x03;
pub const DHGROUP_NULL: u8 = 0x00;

/// Failure reason explanations (`rescode_exp`).
pub const FAIL_FAILED: u8 = 0x01;
pub const FAIL_CONCAT_MISMATCH: u8 = 0x03;
pub const FAIL_HASH_UNUSABLE: u8 = 0x04;
pub const FAIL_DHGROUP_UNUSABLE: u8 = 0x05;
pub const FAIL_INCORRECT_PAYLOAD: u8 = 0x06;
pub const FAIL_INCORRECT_MESSAGE: u8 = 0x07;

/// Digest length of a hash id, or `None` for one this side does not know.
pub fn hash_len(id: u8) -> Option<usize> {
    match id {
        HASH_SHA256 => Some(32),
        HASH_SHA384 => Some(48),
        HASH_SHA512 => Some(64),
        _ => None,
    }
}

fn hmac_with<D: Digest>(block: usize, key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut k = if key.len() > block { D::digest(key).to_vec() } else { key.to_vec() };
    k.resize(block, 0);
    let mut inner = D::new();
    inner.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    for p in parts {
        inner.update(p);
    }
    let ih = inner.finalize();
    let mut outer = D::new();
    outer.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(&ih);
    outer.finalize().to_vec()
}

/// HMAC with the hash `id` names over the concatenation of `parts`.
pub fn hmac(id: u8, key: &[u8], parts: &[&[u8]]) -> Option<Vec<u8>> {
    match id {
        HASH_SHA256 => Some(hmac_with::<sha2::Sha256>(64, key, parts)),
        HASH_SHA384 => Some(hmac_with::<sha2::Sha384>(128, key, parts)),
        HASH_SHA512 => Some(hmac_with::<sha2::Sha512>(128, key, parts)),
        _ => None,
    }
}

/// Compare two byte strings in time that does not depend on where they differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A DH-HMAC-CHAP secret.
#[derive(Clone, PartialEq, Eq)]
pub struct DhchapKey {
    key: Vec<u8>,
    /// The hash that transforms the key with an NQN before use; 0 = none.
    transform: u8,
}

impl std::fmt::Debug for DhchapKey {
    // A secret never reaches a log line through `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DhchapKey(<{} bytes>, hmac {})", self.key.len(), self.transform)
    }
}

impl DhchapKey {
    /// A fresh random 32-byte key, transformed with SHA-256 — what
    /// `nvme gen-dhchap-key --hmac=1` makes.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut key = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        DhchapKey { key, transform: HASH_SHA256 }
    }

    /// Parse `DHHC-1:<hh>:<base64>:` (the trailing colon optional), checking
    /// the key's length and its CRC the way the kernel does.
    pub fn parse(secret: &str) -> Result<Self, String> {
        use base64::Engine;
        let rest = secret
            .trim()
            .strip_prefix("DHHC-1:")
            .ok_or("a DH-HMAC-CHAP secret starts with DHHC-1:")?;
        let (hh, b64) = rest.split_once(':').ok_or("DHHC-1:<hh>:<key>: expected")?;
        let transform: u8 = hh.parse().map_err(|_| format!("bad hash id {hh:?}"))?;
        if transform > 3 {
            return Err(format!("unknown hash id {transform}"));
        }
        let b64 = b64.strip_suffix(':').unwrap_or(b64);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("secret is not base64: {e}"))?;
        if ![36, 52, 68].contains(&raw.len()) {
            return Err(format!("key is {} bytes; 32, 48 or 64 (+4 CRC) expected", raw.len()));
        }
        let (key, crc) = raw.split_at(raw.len() - 4);
        if crc32fast::hash(key).to_le_bytes() != crc {
            return Err("secret's CRC does not match its key".into());
        }
        Ok(DhchapKey { key: key.to_vec(), transform })
    }

    /// The secret in the form `nvme connect --dhchap-secret` takes.
    pub fn to_secret(&self) -> String {
        use base64::Engine;
        let mut raw = self.key.clone();
        raw.extend_from_slice(&crc32fast::hash(&self.key).to_le_bytes());
        format!(
            "DHHC-1:{:02}:{}:",
            self.transform,
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    /// The key as used for one NQN: HMAC(key, nqn ‖ "NVMe-over-Fabrics")
    /// under the secret's own hash, or the key itself for hash 0.
    pub fn transformed(&self, nqn: &str) -> Vec<u8> {
        if self.transform == 0 {
            return self.key.clone();
        }
        hmac(self.transform, &self.key, &[nqn.as_bytes(), b"NVMe-over-Fabrics"])
            .unwrap_or_else(|| self.key.clone())
    }
}

/// The host's response to a challenge (NULL DH group):
/// HMAC(Kh, C1 ‖ S1 ‖ T_ID ‖ SC_C ‖ "HostHost" ‖ hostnqn ‖ 0 ‖ subnqn).
#[allow(clippy::too_many_arguments)]
pub fn host_response(
    hash: u8,
    key: &DhchapKey,
    challenge: &[u8],
    seqnum: u32,
    tid: u16,
    sc_c: u8,
    hostnqn: &str,
    subnqn: &str,
) -> Option<Vec<u8>> {
    let kh = key.transformed(hostnqn);
    hmac(
        hash,
        &kh,
        &[
            challenge,
            &seqnum.to_le_bytes(),
            &tid.to_le_bytes(),
            &[sc_c],
            b"HostHost",
            hostnqn.as_bytes(),
            &[0],
            subnqn.as_bytes(),
        ],
    )
}

/// Where a queue is in the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Waiting for the host's Negotiate.
    Negotiate,
    /// Negotiated; the next Receive returns the challenge.
    Challenge,
    /// Challenged; the next Send is the host's reply.
    Reply,
    /// Verified; the next Receive returns Success1.
    Success1,
    /// Finished and authenticated.
    Done,
    /// Failed; the next Receive returns Failure1 and the queue ends.
    Failure1,
}

/// One queue's side of a DH-HMAC-CHAP exchange, on the controller.
#[derive(Debug)]
pub struct ControllerAuth {
    key: DhchapKey,
    hostnqn: String,
    subnqn: String,
    step: Step,
    tid: u16,
    hash: u8,
    c1: Vec<u8>,
    s1: u32,
    status: u8,
    authenticated: bool,
}

/// What a Receive produced, and whether the queue must now end.
pub struct Received {
    pub data: Vec<u8>,
    pub fatal: bool,
}

impl ControllerAuth {
    pub fn new(key: DhchapKey, hostnqn: &str, subnqn: &str) -> Self {
        ControllerAuth {
            key,
            hostnqn: hostnqn.to_string(),
            subnqn: subnqn.to_string(),
            step: Step::Negotiate,
            tid: 0,
            hash: 0,
            c1: Vec::new(),
            s1: 0,
            status: 0,
            authenticated: false,
        }
    }

    pub fn authenticated(&self) -> bool {
        self.authenticated
    }

    fn fail(&mut self, why: u8) {
        self.step = Step::Failure1;
        self.status = why;
        self.authenticated = false;
    }

    /// An Authentication Send's payload.
    pub fn send(&mut self, d: &[u8]) {
        if d.len() < 2 {
            return self.fail(FAIL_INCORRECT_PAYLOAD);
        }
        let (auth_type, auth_id) = (d[0], d[1]);
        match (auth_type, auth_id) {
            (AUTH_COMMON, MSG_NEGOTIATE) => {
                // A Negotiate restarts the exchange whatever came before.
                self.authenticated = false;
                match self.negotiate(d) {
                    Ok(()) => self.step = Step::Challenge,
                    Err(why) => self.fail(why),
                }
            }
            (AUTH_DHCHAP, MSG_REPLY) if self.step == Step::Reply => {
                if u16::from_le_bytes([d[4], d[5]]) != self.tid {
                    return self.fail(FAIL_INCORRECT_PAYLOAD);
                }
                match self.reply(d) {
                    Ok(()) => self.step = Step::Success1,
                    Err(why) => self.fail(why),
                }
            }
            (AUTH_COMMON, MSG_FAILURE2) | (AUTH_DHCHAP, MSG_FAILURE2) => {
                self.fail(d.get(7).copied().unwrap_or(FAIL_FAILED))
            }
            _ => self.fail(FAIL_INCORRECT_MESSAGE),
        }
    }

    fn negotiate(&mut self, d: &[u8]) -> Result<(), u8> {
        // 8-byte header + one 64-byte protocol descriptor.
        if d.len() < 8 + 64 {
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        self.tid = u16::from_le_bytes([d[4], d[5]]);
        let sc_c = d[6];
        let napd = d[7];
        if sc_c != 0 {
            // Secure concatenation needs TLS, which this transport has not.
            return Err(FAIL_CONCAT_MISMATCH);
        }
        if napd != 1 {
            return Err(FAIL_HASH_UNUSABLE);
        }
        let desc = &d[8..8 + 64];
        if desc[0] != DHCHAP_AUTH_ID {
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        let (halen, dhlen) = (desc[2] as usize, desc[3] as usize);
        if halen > 30 || dhlen > 30 {
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        let idlist = &desc[4..64];
        self.hash = idlist[..halen]
            .iter()
            .copied()
            .find(|h| hash_len(*h).is_some())
            .ok_or(FAIL_HASH_UNUSABLE)?;
        if !idlist[30..30 + dhlen].contains(&DHGROUP_NULL) {
            return Err(FAIL_DHGROUP_UNUSABLE);
        }
        Ok(())
    }

    fn reply(&mut self, d: &[u8]) -> Result<(), u8> {
        if d.len() < 16 {
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        let hl = d[6] as usize;
        let cvalid = d[8];
        let dhvlen = u16::from_le_bytes([d[10], d[11]]) as usize;
        let seqnum = u32::from_le_bytes([d[12], d[13], d[14], d[15]]);
        if Some(hl) != hash_len(self.hash) || 16 + 2 * hl + dhvlen > d.len() {
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        if dhvlen != 0 {
            // A DH value under the NULL group is a malformed reply.
            return Err(FAIL_INCORRECT_PAYLOAD);
        }
        let expect = host_response(
            self.hash, &self.key, &self.c1, self.s1, self.tid, 0, &self.hostnqn, &self.subnqn,
        )
        .ok_or(FAIL_HASH_UNUSABLE)?;
        if !ct_eq(&d[16..16 + hl], &expect) {
            return Err(FAIL_FAILED);
        }
        if cvalid != 0 && seqnum != 0 {
            // The host wants the controller to prove itself too, and this
            // engine keeps no controller secret: refuse, as Linux does.
            return Err(FAIL_FAILED);
        }
        self.authenticated = true;
        Ok(())
    }

    /// An Authentication Receive of `al` bytes.
    pub fn receive(&mut self, al: usize) -> Received {
        let mut out = match self.step {
            Step::Challenge => {
                use rand::RngCore;
                let hl = hash_len(self.hash).unwrap_or(32);
                let mut c1 = vec![0u8; hl];
                rand::rngs::OsRng.fill_bytes(&mut c1);
                // A sequence number of zero means "not bidirectional" in a
                // reply, so the controller never picks it.
                let mut s1 = 0u32;
                while s1 == 0 {
                    s1 = rand::rngs::OsRng.next_u32();
                }
                let mut v = vec![0u8; 16];
                v[0] = AUTH_DHCHAP;
                v[1] = MSG_CHALLENGE;
                v[4..6].copy_from_slice(&self.tid.to_le_bytes());
                v[6] = hl as u8;
                v[8] = self.hash;
                v[9] = DHGROUP_NULL;
                // dhvlen (10..12) = 0
                v[12..16].copy_from_slice(&s1.to_le_bytes());
                v.extend_from_slice(&c1);
                self.c1 = c1;
                self.s1 = s1;
                self.step = Step::Reply;
                v
            }
            Step::Success1 => {
                let mut v = vec![0u8; 16];
                v[0] = AUTH_DHCHAP;
                v[1] = MSG_SUCCESS1;
                v[4..6].copy_from_slice(&self.tid.to_le_bytes());
                v[6] = hash_len(self.hash).unwrap_or(0) as u8;
                // rvalid (8) = 0: no controller response.
                self.step = Step::Done;
                v
            }
            _ => {
                if self.step != Step::Failure1 {
                    self.fail(FAIL_INCORRECT_MESSAGE);
                }
                let mut v = vec![0u8; 8];
                v[0] = AUTH_COMMON;
                v[1] = MSG_FAILURE1;
                v[4..6].copy_from_slice(&self.tid.to_le_bytes());
                v[6] = 0x01; // reason: failed
                v[7] = self.status;
                v.resize(al, 0);
                return Received { data: v, fatal: true };
            }
        };
        // Exactly what the host asked for, as Linux's target answers: the
        // transfer is `al` bytes, zero-padded.
        out.resize(al, 0);
        Received { data: out, fatal: false }
    }
}

/// The host's side, for this engine's own initiator: the Negotiate it sends.
pub fn host_negotiate(tid: u16) -> Vec<u8> {
    let mut v = vec![0u8; 8 + 64];
    v[0] = AUTH_COMMON;
    v[1] = MSG_NEGOTIATE;
    v[4..6].copy_from_slice(&tid.to_le_bytes());
    v[6] = 0; // no secure concatenation
    v[7] = 1; // one protocol descriptor
    v[8] = DHCHAP_AUTH_ID;
    v[10] = 3; // halen
    v[11] = 1; // dhlen
    v[12] = HASH_SHA256;
    v[13] = HASH_SHA384;
    v[14] = HASH_SHA512;
    v[12 + 30] = DHGROUP_NULL;
    v
}

/// The host's side: turn a challenge into the reply it sends.
pub fn host_reply(challenge: &[u8], key: &DhchapKey, hostnqn: &str, subnqn: &str) -> Result<Vec<u8>, String> {
    if challenge.len() < 16 {
        return Err("short challenge".into());
    }
    if challenge[0] == AUTH_COMMON && challenge[1] == MSG_FAILURE1 {
        return Err(format!("controller refused the negotiation (reason {:#x})", challenge[7]));
    }
    if challenge[0] != AUTH_DHCHAP || challenge[1] != MSG_CHALLENGE {
        return Err(format!("expected a challenge, got {:#x}/{:#x}", challenge[0], challenge[1]));
    }
    let tid = u16::from_le_bytes([challenge[4], challenge[5]]);
    let hl = challenge[6] as usize;
    let hash = challenge[8];
    if challenge[9] != DHGROUP_NULL {
        return Err(format!("controller chose DH group {}, only NULL is supported", challenge[9]));
    }
    let s1 = u32::from_le_bytes([challenge[12], challenge[13], challenge[14], challenge[15]]);
    if hash_len(hash) != Some(hl) || challenge.len() < 16 + hl {
        return Err("malformed challenge".into());
    }
    let r = host_response(hash, key, &challenge[16..16 + hl], s1, tid, 0, hostnqn, subnqn)
        .ok_or("unknown hash")?;
    let mut v = vec![0u8; 16];
    v[0] = AUTH_DHCHAP;
    v[1] = MSG_REPLY;
    v[4..6].copy_from_slice(&tid.to_le_bytes());
    v[6] = hl as u8;
    // cvalid (8) = 0, dhvlen = 0, seqnum = 0: unidirectional.
    v.extend_from_slice(&r);
    v.extend(std::iter::repeat(0u8).take(hl)); // the unused C2 slot
    Ok(v)
}

/// The host's side: check the controller's last word.
pub fn host_check_success1(d: &[u8]) -> Result<(), String> {
    match (d.first(), d.get(1)) {
        (Some(&AUTH_DHCHAP), Some(&MSG_SUCCESS1)) => Ok(()),
        (Some(&AUTH_COMMON), Some(&MSG_FAILURE1)) => Err(format!(
            "authentication failed (reason {:#x})",
            d.get(7).copied().unwrap_or(0)
        )),
        _ => Err("unexpected authentication message".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// RFC 4231 test case 1 and 2, all three hashes: the HMAC is ours, so
    /// something other than ourselves has to say it is right.
    #[test]
    fn hmac_matches_rfc4231() {
        let key = vec![0x0bu8; 20];
        let data = b"Hi There";
        assert_eq!(
            hmac(HASH_SHA256, &key, &[data]).unwrap(),
            unhex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
        assert_eq!(
            hmac(HASH_SHA384, &key, &[data]).unwrap(),
            unhex("afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6")
        );
        assert_eq!(
            hmac(HASH_SHA512, &key, &[data]).unwrap(),
            unhex("87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854")
        );
        // Case 2, split across parts: the concatenation is what counts.
        assert_eq!(
            hmac(HASH_SHA256, b"Jefe", &[b"what do ya want ", b"for nothing?"]).unwrap(),
            unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        // Case 6: a key longer than the block is hashed first.
        let long = vec![0xaau8; 131];
        assert_eq!(
            hmac(HASH_SHA256, &long, &[b"Test Using Larger Than Block-Size Key - Hash Key First"]).unwrap(),
            unhex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn secret_round_trips_and_checks_its_crc() {
        let k = DhchapKey::generate();
        let s = k.to_secret();
        assert!(s.starts_with("DHHC-1:01:") && s.ends_with(':'));
        assert_eq!(DhchapKey::parse(&s).unwrap(), k);
        // Without the trailing colon too, as nvme-cli accepts.
        assert_eq!(DhchapKey::parse(s.trim_end_matches(':')).unwrap(), k);
        // One flipped key bit fails the CRC.
        use base64::Engine;
        let b64 = s.trim_start_matches("DHHC-1:01:").trim_end_matches(':');
        let mut raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        raw[0] ^= 1;
        let bad = format!("DHHC-1:01:{}:", base64::engine::general_purpose::STANDARD.encode(raw));
        assert!(DhchapKey::parse(&bad).unwrap_err().contains("CRC"));
        assert!(DhchapKey::parse("DHHC-1:09:AAAA:").is_err());
        assert!(DhchapKey::parse("nope").is_err());
        // Debug never prints the key.
        assert!(!format!("{k:?}").contains(b64));
    }

    /// A secret nvme-cli made (`nvme gen-dhchap-key` output from the spec's
    /// example format): parses, and a hash-0 key is used as-is.
    #[test]
    fn untransformed_key_is_the_key() {
        let k = DhchapKey { key: vec![7u8; 32], transform: 0 };
        let back = DhchapKey::parse(&k.to_secret()).unwrap();
        assert_eq!(back.transformed("nqn.any"), vec![7u8; 32]);
        let t = DhchapKey { key: vec![7u8; 32], transform: HASH_SHA256 };
        assert_eq!(
            t.transformed("nqn.h"),
            hmac(HASH_SHA256, &[7u8; 32], &[b"nqn.hNVMe-over-Fabrics"]).unwrap()
        );
    }

    fn exchange(ctrl_key: &DhchapKey, host_key: &DhchapKey, host: &str) -> Result<(), String> {
        let sub = "nqn.test:sub";
        let mut c = ControllerAuth::new(ctrl_key.clone(), host, sub);
        c.send(&host_negotiate(7));
        let ch = c.receive(1024);
        assert!(!ch.fatal);
        let reply = host_reply(&ch.data, host_key, host, sub)?;
        c.send(&reply);
        let s1 = c.receive(16);
        host_check_success1(&s1.data)?;
        assert!(c.authenticated());
        Ok(())
    }

    #[test]
    fn right_secret_authenticates_wrong_one_does_not() {
        let k = DhchapKey::generate();
        exchange(&k, &k, "nqn.host:a").unwrap();
        let other = DhchapKey::generate();
        let e = exchange(&k, &other, "nqn.host:a").unwrap_err();
        assert!(e.contains("failed"), "{e}");
    }

    #[test]
    fn nothing_but_negotiate_starts_it() {
        let k = DhchapKey::generate();
        let mut c = ControllerAuth::new(k, "h", "s");
        // A reply out of turn, then a receive: Failure1, fatal.
        c.send(&[AUTH_DHCHAP, MSG_REPLY, 0, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let r = c.receive(16);
        assert!(r.fatal);
        assert_eq!(&r.data[..2], &[AUTH_COMMON, MSG_FAILURE1]);
        assert!(!c.authenticated());
    }

    #[test]
    fn negotiate_without_null_group_is_refused() {
        let mut n = host_negotiate(1);
        n[12 + 30] = 0x01; // only ffdhe2048
        let mut c = ControllerAuth::new(DhchapKey::generate(), "h", "s");
        c.send(&n);
        let r = c.receive(16);
        assert!(r.fatal);
        assert_eq!(r.data[7], FAIL_DHGROUP_UNUSABLE);
    }

    #[test]
    fn bidirectional_is_refused() {
        let k = DhchapKey::generate();
        let mut c = ControllerAuth::new(k.clone(), "h", "s");
        c.send(&host_negotiate(1));
        let ch = c.receive(128);
        let mut reply = host_reply(&ch.data, &k, "h", "s").unwrap();
        reply[8] = 1; // cvalid
        reply[12] = 5; // seqnum
        c.send(&reply);
        assert!(c.receive(16).fatal);
        assert!(!c.authenticated());
    }
}
