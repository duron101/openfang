//! Self-Destruct Guard — cryptographic multi-party verification.
//!
//! Implements the UMAA-aligned self-destruct protocol:
//! - Requires >= 3 HMAC-SHA256 signatures from authorized entities (SC, SO, HEC)
//! - Constant-time signature comparison (subtle crate)
//! - Timestamp validation (< 60s drift to prevent replay)
//! - Whitelist verification of signer identities

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Authorized signer roles for self-destruct
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerRole {
    /// Shore Commander
    SC,
    /// Safety Officer (veto power)
    SO,
    /// Higher Echelon Command
    HEC,
}

/// A self-destruct verification request
#[derive(Debug, Clone)]
pub struct SelfDestructRequest {
    /// Reason for self-destruct
    pub reason: String,
    /// When the command was issued
    pub timestamp: DateTime<Utc>,
    /// (signer identity, HMAC signature hex string)
    pub signatures: Vec<(String, String)>,
}

/// Verification result
#[derive(Debug, Clone, PartialEq)]
pub enum VerificationResult {
    /// All checks passed
    Approved,
    /// Not enough valid signatures
    InsufficientSignatures { required: usize, valid: usize },
    /// Signer not in whitelist
    UnauthorizedSigner(String),
    /// Signature verification failed
    InvalidSignature(String),
    /// Command expired (timestamp too old)
    Expired { age_s: f64, max_age_s: f64 },
    /// Missing required signer roles
    MissingRoles(Vec<SignerRole>),
}

/// The self-destruct guard — stateless verification.
pub struct SelfDestructGuard {
    /// Required minimum signers
    min_signers: usize,
    /// Maximum age of the command in seconds
    max_age_s: f64,
    /// Whitelist of authorized signer identities → shared secrets
    authorized_signers: Vec<(String, String, SignerRole)>,
}

impl SelfDestructGuard {
    /// Create a new self-destruct guard.
    ///
    /// `authorized_signers`: (identity, shared_secret, role) tuples.
    /// At minimum, SC + SO + HEC must be present.
    pub fn new(
        min_signers: usize,
        max_age_s: f64,
        authorized_signers: Vec<(String, String, SignerRole)>,
    ) -> Self {
        Self {
            min_signers,
            max_age_s,
            authorized_signers,
        }
    }

    /// Default: 3 signers (SC + SO + HEC), 60s max age
    pub fn default_config(sc_secret: &str, so_secret: &str, hec_secret: &str) -> Self {
        Self::new(
            3,
            60.0,
            vec![
                ("SC".into(), sc_secret.into(), SignerRole::SC),
                ("SO".into(), so_secret.into(), SignerRole::SO),
                ("HEC".into(), hec_secret.into(), SignerRole::HEC),
            ],
        )
    }

    /// Verify a self-destruct request.
    ///
    /// Checks:
    /// 1. Timestamp freshness (< max_age_s)
    /// 2. Sufficient valid signatures (>= min_signers)
    /// 3. All required roles present (SC + SO + HEC)
    /// 4. HMAC verification for each signature
    pub fn verify(&self, request: &SelfDestructRequest) -> VerificationResult {
        // 1. Timestamp check
        let now = Utc::now();
        let age_s = (now - request.timestamp).num_seconds() as f64;
        if age_s > self.max_age_s {
            return VerificationResult::Expired {
                age_s,
                max_age_s: self.max_age_s,
            };
        }

        // 2. Build the message that was signed
        let message = format!("SELFDESTRUCT:{}:{}", request.reason, request.timestamp);

        // 3. Verify each signature
        let mut valid_count = 0;
        let mut valid_roles: Vec<SignerRole> = Vec::new();

        for (signer_id, sig_hex) in &request.signatures {
            // Find the authorized signer
            let authorized = self
                .authorized_signers
                .iter()
                .find(|(id, _, _)| id == signer_id);

            let (_, secret, role) = match authorized {
                Some(a) => a,
                None => return VerificationResult::UnauthorizedSigner(signer_id.clone()),
            };

            // Verify HMAC
            if !verify_hmac(secret, &message, sig_hex) {
                return VerificationResult::InvalidSignature(signer_id.clone());
            }

            valid_count += 1;
            valid_roles.push(*role);
        }

        // 4. Check minimum signers
        if valid_count < self.min_signers {
            return VerificationResult::InsufficientSignatures {
                required: self.min_signers,
                valid: valid_count,
            };
        }

        // 5. Check required roles
        let required_roles = [SignerRole::SC, SignerRole::SO, SignerRole::HEC];
        let missing: Vec<SignerRole> = required_roles
            .iter()
            .filter(|r| !valid_roles.contains(r))
            .copied()
            .collect();

        if !missing.is_empty() {
            return VerificationResult::MissingRoles(missing);
        }

        VerificationResult::Approved
    }
}

/// HMAC-SHA256 verification with constant-time comparison.
fn verify_hmac(secret: &str, message: &str, signature_hex: &str) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(message.as_bytes());
    let expected = mac.finalize().into_bytes();

    // Decode hex signature
    let decoded = match hex::decode(signature_hex) {
        Ok(d) => d,
        Err(_) => return false,
    };

    // Constant-time comparison
    expected.ct_eq(&decoded).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn sign_message(secret: &str, message: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn test_valid_self_destruct() {
        let guard = SelfDestructGuard::default_config("sc-key", "so-key", "hec-key");
        let now = Utc::now();
        let msg = format!("SELFDESTRUCT:test:{}", now);

        let req = SelfDestructRequest {
            reason: "test".into(),
            timestamp: now,
            signatures: vec![
                ("SC".into(), sign_message("sc-key", &msg)),
                ("SO".into(), sign_message("so-key", &msg)),
                ("HEC".into(), sign_message("hec-key", &msg)),
            ],
        };

        assert_eq!(guard.verify(&req), VerificationResult::Approved);
    }

    #[test]
    fn test_expired_request() {
        let guard = SelfDestructGuard::default_config("sc-key", "so-key", "hec-key");
        let old = Utc::now() - Duration::seconds(120);
        let req = SelfDestructRequest {
            reason: "test".into(),
            timestamp: old,
            signatures: vec![],
        };

        match guard.verify(&req) {
            VerificationResult::Expired { age_s, max_age_s } => {
                assert!(age_s > max_age_s);
            }
            other => panic!("Expected Expired, got {:?}", other),
        }
    }

    #[test]
    fn test_insufficient_signers() {
        let guard = SelfDestructGuard::default_config("sc-key", "so-key", "hec-key");
        let now = Utc::now();
        let msg = format!("SELFDESTRUCT:test:{}", now);

        let req = SelfDestructRequest {
            reason: "test".into(),
            timestamp: now,
            signatures: vec![("SC".into(), sign_message("sc-key", &msg))],
        };

        match guard.verify(&req) {
            VerificationResult::InsufficientSignatures { required, valid } => {
                assert_eq!(required, 3);
                assert_eq!(valid, 1);
            }
            other => panic!("Expected InsufficientSignatures, got {:?}", other),
        }
    }

    #[test]
    fn test_invalid_signature() {
        let guard = SelfDestructGuard::default_config("sc-key", "so-key", "hec-key");
        let now = Utc::now();

        let req = SelfDestructRequest {
            reason: "test".into(),
            timestamp: now,
            signatures: vec![("SC".into(), "bad_signature_hex".into())],
        };

        match guard.verify(&req) {
            VerificationResult::InvalidSignature(id) => assert_eq!(id, "SC"),
            other => panic!("Expected InvalidSignature, got {:?}", other),
        }
    }
}
