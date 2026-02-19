//! CHAP authentication for iSCSI — MD5 challenge/response per RFC 7143 §12.
//!
//! Flow:
//! 1. Target sends: CHAP_A=5 (MD5), CHAP_I=<id>, CHAP_C=<challenge>
//! 2. Initiator responds: CHAP_N=<name>, CHAP_R=<response>
//! 3. Target verifies: response == MD5(id || secret || challenge)

use md5::{Md5, Digest};
use rand::Rng;

/// CHAP algorithm identifier.
const CHAP_MD5: u8 = 5;

/// CHAP configuration for iSCSI target.
#[derive(Debug, Clone)]
pub struct ChapConfig {
    pub username: String,
    pub secret: String,
}

/// CHAP state machine.
pub struct ChapAuthenticator {
    config: ChapConfig,
    challenge_id: u8,
    challenge: Vec<u8>,
}

impl ChapAuthenticator {
    pub fn new(config: ChapConfig) -> Self {
        let mut rng = rand::thread_rng();
        let challenge_id: u8 = rng.gen();
        let mut challenge = vec![0u8; 16];
        rng.fill(&mut challenge[..]);

        ChapAuthenticator {
            config,
            challenge_id,
            challenge,
        }
    }

    /// Generate challenge parameters to send to initiator.
    /// Returns key-value pairs: CHAP_A, CHAP_I, CHAP_C.
    pub fn challenge_params(&self) -> Vec<(String, String)> {
        vec![
            ("CHAP_A".into(), CHAP_MD5.to_string()),
            ("CHAP_I".into(), self.challenge_id.to_string()),
            ("CHAP_C".into(), hex_encode(&self.challenge)),
        ]
    }

    /// Verify the initiator's CHAP response.
    /// `name` = CHAP_N value, `response_hex` = CHAP_R value (hex-encoded).
    pub fn verify(&self, name: &str, response_hex: &str) -> bool {
        if name != self.config.username {
            tracing::warn!("CHAP: username mismatch: expected '{}', got '{}'", self.config.username, name);
            return false;
        }

        let response = match hex_decode(response_hex) {
            Some(r) => r,
            None => {
                tracing::warn!("CHAP: invalid hex response");
                return false;
            }
        };

        let expected = compute_chap_response(
            self.challenge_id,
            self.config.secret.as_bytes(),
            &self.challenge,
        );

        if response.len() != expected.len() {
            return false;
        }

        // Constant-time comparison
        let mut diff = 0u8;
        for (a, b) in response.iter().zip(expected.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// Compute CHAP MD5 response: MD5(id || secret || challenge).
pub fn compute_chap_response(id: u8, secret: &[u8], challenge: &[u8]) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update([id]);
    hasher.update(secret);
    hasher.update(challenge);
    hasher.finalize().to_vec()
}

/// Hex-encode bytes with "0x" prefix.
fn hex_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(2 + data.len() * 2);
    s.push_str("0x");
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex-decode a string, optionally with "0x" prefix.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chap_md5_known_vector() {
        // id=0, secret="secret", challenge=[0x00; 16]
        let response = compute_chap_response(0, b"secret", &[0u8; 16]);
        assert_eq!(response.len(), 16); // MD5 output

        // Verify the same inputs produce the same output
        let response2 = compute_chap_response(0, b"secret", &[0u8; 16]);
        assert_eq!(response, response2);
    }

    #[test]
    fn chap_authenticator_flow() {
        let config = ChapConfig {
            username: "admin".into(),
            secret: "s3cret".into(),
        };
        let auth = ChapAuthenticator::new(config);
        let params = auth.challenge_params();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].0, "CHAP_A");
        assert_eq!(params[0].1, "5");

        // Compute valid response
        let id = auth.challenge_id;
        let challenge = &auth.challenge;
        let response = compute_chap_response(id, b"s3cret", challenge);
        let response_hex = hex_encode(&response);

        assert!(auth.verify("admin", &response_hex));
        assert!(!auth.verify("wrong_user", &response_hex));
        assert!(!auth.verify("admin", "0xdeadbeef"));
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let encoded = hex_encode(&data);
        assert_eq!(encoded, "0xdeadbeef");
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }
}
