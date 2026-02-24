use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredKey {
    pub name: String,
    pub key_hash: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthStore {
    pub keys: Vec<StoredKey>,
}

impl AuthStore {
    /// Load from a JSON file. Returns an empty store if the file doesn't exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let store: AuthStore = serde_json::from_str(&content)?;
        Ok(store)
    }

    /// Write pretty-printed JSON to the given path.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Hash the raw key and check against stored hashes.
    pub fn verify(&self, raw_key: &str) -> bool {
        let hash = hash_key(raw_key);
        self.keys.iter().any(|k| k.key_hash == hash)
    }

    /// Generate a new key with the given name. Returns the raw key (shown only once).
    /// The key is `evo-` + 48 hex chars (192 bits of entropy).
    pub fn generate(&mut self, name: &str) -> String {
        let raw_key = generate_raw_key();
        let key_hash = hash_key(&raw_key);
        let prefix = raw_key[..16.min(raw_key.len())].to_string();

        self.keys.push(StoredKey {
            name: name.to_string(),
            key_hash,
            prefix,
            created_at: Utc::now(),
        });

        raw_key
    }

    /// Revoke a key by name. Returns true if a key was removed.
    pub fn revoke(&mut self, name: &str) -> bool {
        let len_before = self.keys.len();
        self.keys.retain(|k| k.name != name);
        self.keys.len() < len_before
    }
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_raw_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24]; // 24 bytes = 48 hex chars
    rand::rng().fill_bytes(&mut bytes);
    format!("evo-{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_verify() {
        let mut store = AuthStore::default();
        let raw = store.generate("test-key");

        assert!(raw.starts_with("evo-"));
        assert_eq!(raw.len(), 52); // "evo-" (4) + 48 hex chars
        assert!(store.verify(&raw));
        assert!(!store.verify("evo-wrong"));
    }

    #[test]
    fn revoke_key() {
        let mut store = AuthStore::default();
        store.generate("to-revoke");
        assert_eq!(store.keys.len(), 1);

        assert!(store.revoke("to-revoke"));
        assert_eq!(store.keys.len(), 0);

        assert!(!store.revoke("nonexistent"));
    }
}
