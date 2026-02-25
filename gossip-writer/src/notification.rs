use ed25519_dalek::{SigningKey, Signer, SECRET_KEY_LENGTH};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Gossipping notification message format (version 1.0).
///
/// Canonical form for signing: serialize without the `signature` field,
/// keys sorted (serde_json::Value uses BTreeMap so this is automatic),
/// compact JSON, sign the resulting bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipNotification {
    pub version: String,
    pub sender: String,
    pub timestamp: u64,
    pub medium: String,
    pub reason: String,
    pub iris: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// The canonical (signable) subset — same fields minus `signature`.
#[derive(Debug, Serialize)]
struct CanonicalNotification<'a> {
    pub iris: &'a Vec<String>,
    pub medium: &'a str,
    pub reason: &'a str,
    pub sender: &'a str,
    pub timestamp: u64,
    pub version: &'a str,
}

impl GossipNotification {
    /// Build a new unsigned notification.
    pub fn new(sender_pubkey_hex: &str, medium: &str, reason: &str, iris: Vec<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();

        GossipNotification {
            version: "1.0".to_string(),
            sender: sender_pubkey_hex.to_string(),
            timestamp,
            medium: medium.to_string(),
            reason: reason.to_string(),
            iris,
            signature: None,
        }
    }

    /// Return the canonical JSON bytes used for signing.
    /// Keys are alphabetically sorted because we use a struct with fields
    /// defined in alphabetical order.
    fn canonical_bytes(&self) -> Vec<u8> {
        let canonical = CanonicalNotification {
            iris: &self.iris,
            medium: &self.medium,
            reason: &self.reason,
            sender: &self.sender,
            timestamp: self.timestamp,
            version: &self.version,
        };
        serde_json::to_vec(&canonical).expect("canonical JSON serialization should not fail")
    }

    /// Sign the notification in place and return the full JSON payload (with signature).
    pub fn sign(&mut self, signing_key: &SigningKey) -> Vec<u8> {
        let canonical = self.canonical_bytes();
        let sig = signing_key.sign(&canonical);
        self.signature = Some(hex::encode(sig.to_bytes()));
        serde_json::to_vec(self).expect("JSON serialization should not fail")
    }
}

/// Load an ed25519 signing key from a file, or generate a new one if the file
/// does not exist.
pub fn load_or_generate_key(path: &str) -> Result<SigningKey, Box<dyn Error>> {
    let p = Path::new(path);

    if p.exists() {
        let bytes = fs::read(p)?;
        if bytes.len() != SECRET_KEY_LENGTH {
            return Err(format!(
                "Key file {} has {} bytes, expected {}",
                path,
                bytes.len(),
                SECRET_KEY_LENGTH
            )
            .into());
        }
        let mut key_bytes = [0u8; SECRET_KEY_LENGTH];
        key_bytes.copy_from_slice(&bytes);
        Ok(SigningKey::from_bytes(&key_bytes))
    } else {
        // Create parent directories if needed
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let signing_key = SigningKey::generate(&mut rand::rng());
        fs::write(p, signing_key.to_bytes())?;
        eprintln!("Generated new ed25519 key at {}", path);
        Ok(signing_key)
    }
}

/// Return the hex-encoded public key for a signing key.
pub fn pubkey_hex(signing_key: &SigningKey) -> String {
    hex::encode(signing_key.verifying_key().to_bytes())
}

/// Map a PodpingReason capnp enum value to the Gossipping string form.
pub fn reason_to_string(reason: u16) -> &'static str {
    match reason {
        0 => "update",
        1 => "live",
        2 => "liveEnd",
        _ => "update",
    }
}

/// Map a PodpingMedium capnp enum value to the Gossipping string form.
pub fn medium_to_string(medium: u16) -> &'static str {
    match medium {
        0 => "podcast",
        1 => "podcastL",
        2 => "music",
        3 => "musicL",
        4 => "video",
        5 => "videoL",
        6 => "film",
        7 => "filmL",
        8 => "audiobook",
        9 => "audiobookL",
        10 => "newsletter",
        11 => "newsletterL",
        12 => "blog",
        13 => "blogL",
        14 => "publisher",
        15 => "publisherL",
        16 => "course",
        17 => "courseL",
        _ => "podcast",
    }
}
