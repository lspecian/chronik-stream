//! Backup encryption support

use crate::{BackupError, Result};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce as AesNonce,
};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaPolyNonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// Helper to create HMAC instance (avoids trait method conflicts)
fn create_hmac(key: &[u8]) -> std::result::Result<HmacSha256, BackupError> {
    <HmacSha256 as Mac>::new_from_slice(key)
        .map_err(|e| BackupError::Encryption(format!("Failed to create HMAC: {}", e)))
}

/// Encryption configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    /// Encryption type
    pub encryption_type: EncryptionType,
    /// Key ID or reference
    pub key_id: String,
    /// Key derivation function parameters (for password-based encryption)
    pub kdf_params: Option<KdfParams>,
    /// Additional authenticated data (AAD)
    pub aad: Option<Vec<u8>>,
}

/// Supported encryption types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptionType {
    /// AES-256-GCM (recommended)
    Aes256Gcm,
    /// ChaCha20-Poly1305
    ChaCha20Poly1305,
    /// AES-256-CTR with HMAC-SHA256
    Aes256CtrHmacSha256,
}

impl EncryptionType {
    /// Get the key size in bytes
    pub fn key_size(&self) -> usize {
        match self {
            Self::Aes256Gcm => 32,
            Self::ChaCha20Poly1305 => 32,
            Self::Aes256CtrHmacSha256 => 32,
        }
    }
    
    /// Get the nonce/IV size in bytes
    pub fn nonce_size(&self) -> usize {
        match self {
            Self::Aes256Gcm => 12,
            Self::ChaCha20Poly1305 => 12,
            Self::Aes256CtrHmacSha256 => 16,
        }
    }
    
    /// Get the authentication tag size in bytes
    pub fn tag_size(&self) -> usize {
        match self {
            Self::Aes256Gcm => 16,
            Self::ChaCha20Poly1305 => 16,
            Self::Aes256CtrHmacSha256 => 32, // HMAC-SHA256
        }
    }
}

impl fmt::Display for EncryptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aes256Gcm => write!(f, "AES-256-GCM"),
            Self::ChaCha20Poly1305 => write!(f, "ChaCha20-Poly1305"),
            Self::Aes256CtrHmacSha256 => write!(f, "AES-256-CTR-HMAC-SHA256"),
        }
    }
}

/// Key derivation function parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdfParams {
    /// KDF algorithm
    pub algorithm: KdfAlgorithm,
    /// Salt
    pub salt: Vec<u8>,
    /// Iterations (for PBKDF2)
    pub iterations: Option<u32>,
    /// Memory cost (for Argon2)
    pub memory_cost: Option<u32>,
    /// Time cost (for Argon2)
    pub time_cost: Option<u32>,
    /// Parallelism (for Argon2)
    pub parallelism: Option<u32>,
}

/// Key derivation function algorithms
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KdfAlgorithm {
    /// PBKDF2 with SHA-256
    Pbkdf2Sha256,
    /// Argon2id (recommended for password-based encryption)
    Argon2id,
    /// scrypt
    Scrypt,
}

/// Encrypted data envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedData {
    /// Encryption type used
    pub encryption_type: EncryptionType,
    /// Key ID that was used
    pub key_id: String,
    /// Nonce/IV
    pub nonce: Vec<u8>,
    /// Encrypted data
    pub ciphertext: Vec<u8>,
    /// Authentication tag (for AEAD ciphers)
    pub tag: Option<Vec<u8>>,
    /// KDF parameters (if password-based)
    pub kdf_params: Option<KdfParams>,
}

/// Encrypt data using the specified configuration and key manager
pub fn encrypt(
    data: &[u8],
    config: &EncryptionConfig,
    key_manager: &dyn KeyManager,
) -> Result<Vec<u8>> {
    // Retrieve the key
    let key = key_manager.get_key(&config.key_id)?;

    // Validate key size
    if key.len() != config.encryption_type.key_size() {
        return Err(BackupError::Encryption(format!(
            "Invalid key size: expected {}, got {}",
            config.encryption_type.key_size(),
            key.len()
        )));
    }

    // Generate random nonce
    let nonce = utils::generate_nonce(config.encryption_type.nonce_size());

    // Encrypt based on type
    let (ciphertext, tag) = match config.encryption_type {
        EncryptionType::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(&key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce_arr = AesNonce::from_slice(&nonce);

            // Include AAD if provided
            let encrypted = if let Some(aad) = &config.aad {
                use aes_gcm::aead::Payload;
                cipher
                    .encrypt(nonce_arr, Payload { msg: data, aad })
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            } else {
                cipher
                    .encrypt(nonce_arr, data)
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            };

            // AES-GCM appends tag to ciphertext, extract it
            let tag_start = encrypted.len() - config.encryption_type.tag_size();
            let tag = encrypted[tag_start..].to_vec();
            let ciphertext = encrypted[..tag_start].to_vec();

            (ciphertext, Some(tag))
        }

        EncryptionType::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(&key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce_arr = ChaChaPolyNonce::from_slice(&nonce);

            let encrypted = if let Some(aad) = &config.aad {
                use chacha20poly1305::aead::Payload;
                cipher
                    .encrypt(nonce_arr, Payload { msg: data, aad })
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            } else {
                cipher
                    .encrypt(nonce_arr, data)
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            };

            // ChaCha20-Poly1305 also appends tag
            let tag_start = encrypted.len() - config.encryption_type.tag_size();
            let tag = encrypted[tag_start..].to_vec();
            let ciphertext = encrypted[..tag_start].to_vec();

            (ciphertext, Some(tag))
        }

        EncryptionType::Aes256CtrHmacSha256 => {
            // For CTR mode with HMAC, we need separate encrypt-then-MAC
            use aes::cipher::{KeyIvInit, StreamCipher};
            type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;

            // CTR encryption
            let mut cipher = Aes256Ctr::new_from_slices(&key, &nonce)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;

            let mut ciphertext = data.to_vec();
            cipher.apply_keystream(&mut ciphertext);

            // HMAC-SHA256 over (nonce || ciphertext || aad)
            let mut mac = create_hmac(&key)?;
            mac.update(&nonce);
            mac.update(&ciphertext);
            if let Some(aad) = &config.aad {
                mac.update(aad);
            }
            let tag = mac.finalize().into_bytes().to_vec();

            (ciphertext, Some(tag))
        }
    };

    // Create encrypted data envelope
    let envelope = EncryptedData {
        encryption_type: config.encryption_type,
        key_id: config.key_id.clone(),
        nonce,
        ciphertext,
        tag,
        kdf_params: config.kdf_params.clone(),
    };

    // Serialize envelope
    bincode::serialize(&envelope)
        .map_err(|e| BackupError::Encryption(format!("Failed to serialize envelope: {}", e)))
}

/// Decrypt data using the specified configuration and key manager
pub fn decrypt(
    encrypted_data: &[u8],
    config: &EncryptionConfig,
    key_manager: &dyn KeyManager,
) -> Result<Vec<u8>> {
    // Deserialize envelope
    let envelope: EncryptedData = bincode::deserialize(encrypted_data)
        .map_err(|e| BackupError::Encryption(format!("Failed to deserialize envelope: {}", e)))?;

    // Verify key ID matches
    if envelope.key_id != config.key_id {
        return Err(BackupError::Encryption(format!(
            "Key ID mismatch: expected '{}', got '{}'",
            config.key_id, envelope.key_id
        )));
    }

    // Retrieve the key
    let key = key_manager.get_key(&config.key_id)?;

    // Validate key size
    if key.len() != envelope.encryption_type.key_size() {
        return Err(BackupError::Encryption(format!(
            "Invalid key size: expected {}, got {}",
            envelope.encryption_type.key_size(),
            key.len()
        )));
    }

    // Decrypt based on type
    match envelope.encryption_type {
        EncryptionType::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(&key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce = AesNonce::from_slice(&envelope.nonce);

            // Reconstruct ciphertext with tag appended
            let mut ciphertext_with_tag = envelope.ciphertext.clone();
            if let Some(tag) = &envelope.tag {
                ciphertext_with_tag.extend_from_slice(tag);
            }

            if let Some(aad) = &config.aad {
                use aes_gcm::aead::Payload;
                cipher
                    .decrypt(
                        nonce,
                        Payload {
                            msg: &ciphertext_with_tag,
                            aad,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            } else {
                cipher
                    .decrypt(nonce, ciphertext_with_tag.as_slice())
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            }
        }

        EncryptionType::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(&key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce = ChaChaPolyNonce::from_slice(&envelope.nonce);

            // Reconstruct ciphertext with tag
            let mut ciphertext_with_tag = envelope.ciphertext.clone();
            if let Some(tag) = &envelope.tag {
                ciphertext_with_tag.extend_from_slice(tag);
            }

            if let Some(aad) = &config.aad {
                use chacha20poly1305::aead::Payload;
                cipher
                    .decrypt(
                        nonce,
                        Payload {
                            msg: &ciphertext_with_tag,
                            aad,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            } else {
                cipher
                    .decrypt(nonce, ciphertext_with_tag.as_slice())
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            }
        }

        EncryptionType::Aes256CtrHmacSha256 => {
            use aes::cipher::{KeyIvInit, StreamCipher};
            type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;

            // Verify HMAC first (authenticate-then-decrypt)
            let expected_tag = envelope.tag.as_ref().ok_or_else(|| {
                BackupError::Encryption("Missing HMAC tag for CTR mode".to_string())
            })?;

            let mut mac = create_hmac(&key)?;
            mac.update(&envelope.nonce);
            mac.update(&envelope.ciphertext);
            if let Some(aad) = &config.aad {
                mac.update(aad);
            }

            mac.verify_slice(expected_tag)
                .map_err(|_| BackupError::Encryption("HMAC verification failed".to_string()))?;

            // Decrypt
            let mut cipher = Aes256Ctr::new_from_slices(&key, &envelope.nonce)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;

            let mut plaintext = envelope.ciphertext.clone();
            cipher.apply_keystream(&mut plaintext);

            Ok(plaintext)
        }
    }
}

/// Simple encryption without KeyManager (uses raw key directly)
pub fn encrypt_with_key(
    data: &[u8],
    key: &[u8],
    encryption_type: EncryptionType,
    aad: Option<&[u8]>,
) -> Result<EncryptedData> {
    // Validate key size
    if key.len() != encryption_type.key_size() {
        return Err(BackupError::Encryption(format!(
            "Invalid key size: expected {}, got {}",
            encryption_type.key_size(),
            key.len()
        )));
    }

    // Generate random nonce
    let nonce = utils::generate_nonce(encryption_type.nonce_size());

    // Encrypt based on type
    let (ciphertext, tag) = match encryption_type {
        EncryptionType::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce_arr = AesNonce::from_slice(&nonce);

            let encrypted = if let Some(aad_data) = aad {
                use aes_gcm::aead::Payload;
                cipher
                    .encrypt(
                        nonce_arr,
                        Payload {
                            msg: data,
                            aad: aad_data,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            } else {
                cipher
                    .encrypt(nonce_arr, data)
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            };

            let tag_start = encrypted.len() - encryption_type.tag_size();
            (encrypted[..tag_start].to_vec(), Some(encrypted[tag_start..].to_vec()))
        }

        EncryptionType::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce_arr = ChaChaPolyNonce::from_slice(&nonce);

            let encrypted = if let Some(aad_data) = aad {
                use chacha20poly1305::aead::Payload;
                cipher
                    .encrypt(
                        nonce_arr,
                        Payload {
                            msg: data,
                            aad: aad_data,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            } else {
                cipher
                    .encrypt(nonce_arr, data)
                    .map_err(|e| BackupError::Encryption(format!("Encryption failed: {}", e)))?
            };

            let tag_start = encrypted.len() - encryption_type.tag_size();
            (encrypted[..tag_start].to_vec(), Some(encrypted[tag_start..].to_vec()))
        }

        EncryptionType::Aes256CtrHmacSha256 => {
            use aes::cipher::{KeyIvInit, StreamCipher};
            type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;

            let mut cipher = Aes256Ctr::new_from_slices(key, &nonce)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;

            let mut ciphertext_data = data.to_vec();
            cipher.apply_keystream(&mut ciphertext_data);

            let mut mac = create_hmac(key)?;
            mac.update(&nonce);
            mac.update(&ciphertext_data);
            if let Some(aad_data) = aad {
                mac.update(aad_data);
            }
            let tag_data = mac.finalize().into_bytes().to_vec();

            (ciphertext_data, Some(tag_data))
        }
    };

    Ok(EncryptedData {
        encryption_type,
        key_id: String::new(),
        nonce,
        ciphertext,
        tag,
        kdf_params: None,
    })
}

/// Simple decryption without KeyManager (uses raw key directly)
pub fn decrypt_with_key(
    envelope: &EncryptedData,
    key: &[u8],
    aad: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // Validate key size
    if key.len() != envelope.encryption_type.key_size() {
        return Err(BackupError::Encryption(format!(
            "Invalid key size: expected {}, got {}",
            envelope.encryption_type.key_size(),
            key.len()
        )));
    }

    match envelope.encryption_type {
        EncryptionType::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce = AesNonce::from_slice(&envelope.nonce);

            let mut ciphertext_with_tag = envelope.ciphertext.clone();
            if let Some(tag) = &envelope.tag {
                ciphertext_with_tag.extend_from_slice(tag);
            }

            if let Some(aad_data) = aad {
                use aes_gcm::aead::Payload;
                cipher
                    .decrypt(
                        nonce,
                        Payload {
                            msg: &ciphertext_with_tag,
                            aad: aad_data,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            } else {
                cipher
                    .decrypt(nonce, ciphertext_with_tag.as_slice())
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            }
        }

        EncryptionType::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(key)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;
            let nonce = ChaChaPolyNonce::from_slice(&envelope.nonce);

            let mut ciphertext_with_tag = envelope.ciphertext.clone();
            if let Some(tag) = &envelope.tag {
                ciphertext_with_tag.extend_from_slice(tag);
            }

            if let Some(aad_data) = aad {
                use chacha20poly1305::aead::Payload;
                cipher
                    .decrypt(
                        nonce,
                        Payload {
                            msg: &ciphertext_with_tag,
                            aad: aad_data,
                        },
                    )
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            } else {
                cipher
                    .decrypt(nonce, ciphertext_with_tag.as_slice())
                    .map_err(|e| BackupError::Encryption(format!("Decryption failed: {}", e)))
            }
        }

        EncryptionType::Aes256CtrHmacSha256 => {
            use aes::cipher::{KeyIvInit, StreamCipher};
            type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;

            let expected_tag = envelope.tag.as_ref().ok_or_else(|| {
                BackupError::Encryption("Missing HMAC tag for CTR mode".to_string())
            })?;

            let mut mac = create_hmac(key)?;
            mac.update(&envelope.nonce);
            mac.update(&envelope.ciphertext);
            if let Some(aad_data) = aad {
                mac.update(aad_data);
            }

            mac.verify_slice(expected_tag)
                .map_err(|_| BackupError::Encryption("HMAC verification failed".to_string()))?;

            let mut cipher = Aes256Ctr::new_from_slices(key, &envelope.nonce)
                .map_err(|e| BackupError::Encryption(format!("Failed to create cipher: {}", e)))?;

            let mut plaintext = envelope.ciphertext.clone();
            cipher.apply_keystream(&mut plaintext);

            Ok(plaintext)
        }
    }
}

/// Key management interface
pub trait KeyManager: Send + Sync {
    /// Retrieve a key by ID
    fn get_key(&self, key_id: &str) -> Result<Vec<u8>>;
    
    /// Store a key with ID
    fn store_key(&self, key_id: &str, key: &[u8]) -> Result<()>;
    
    /// Delete a key
    fn delete_key(&self, key_id: &str) -> Result<()>;
    
    /// List available key IDs
    fn list_keys(&self) -> Result<Vec<String>>;
    
    /// Generate a new key
    fn generate_key(&self, key_id: &str, key_type: EncryptionType) -> Result<()>;
    
    /// Rotate a key (create new version)
    fn rotate_key(&self, key_id: &str) -> Result<String>;
}

/// In-memory key manager (for testing only!)
pub struct MemoryKeyManager {
    keys: std::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>,
}

impl MemoryKeyManager {
    /// Create a new memory-based key manager
    pub fn new() -> Self {
        Self {
            keys: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl KeyManager for MemoryKeyManager {
    fn get_key(&self, key_id: &str) -> Result<Vec<u8>> {
        self.keys.read().unwrap()
            .get(key_id)
            .cloned()
            .ok_or_else(|| BackupError::Encryption(format!("Key not found: {}", key_id)))
    }
    
    fn store_key(&self, key_id: &str, key: &[u8]) -> Result<()> {
        self.keys.write().unwrap()
            .insert(key_id.to_string(), key.to_vec());
        Ok(())
    }
    
    fn delete_key(&self, key_id: &str) -> Result<()> {
        self.keys.write().unwrap()
            .remove(key_id)
            .ok_or_else(|| BackupError::Encryption(format!("Key not found: {}", key_id)))?;
        Ok(())
    }
    
    fn list_keys(&self) -> Result<Vec<String>> {
        Ok(self.keys.read().unwrap()
            .keys()
            .cloned()
            .collect())
    }
    
    fn generate_key(&self, key_id: &str, key_type: EncryptionType) -> Result<()> {
        use rand::RngCore;
        
        let key_size = key_type.key_size();
        let mut key = vec![0u8; key_size];
        rand::thread_rng().fill_bytes(&mut key);
        
        self.store_key(key_id, &key)
    }
    
    fn rotate_key(&self, key_id: &str) -> Result<String> {
        // Get existing key type (assume AES-256-GCM for now)
        let old_key = self.get_key(key_id)?;
        
        // Generate new key ID
        let new_key_id = format!("{}-v{}", key_id, chrono::Utc::now().timestamp());
        
        // Generate new key
        self.generate_key(&new_key_id, EncryptionType::Aes256Gcm)?;
        
        Ok(new_key_id)
    }
}

/// Encryption utilities
pub mod utils {
    use super::*;
    use rand::RngCore;
    
    /// Generate a random nonce
    pub fn generate_nonce(size: usize) -> Vec<u8> {
        let mut nonce = vec![0u8; size];
        rand::thread_rng().fill_bytes(&mut nonce);
        nonce
    }
    
    /// Generate a random salt for KDF
    pub fn generate_salt(size: usize) -> Vec<u8> {
        let mut salt = vec![0u8; size];
        rand::thread_rng().fill_bytes(&mut salt);
        salt
    }
    
    /// Derive key from password using PBKDF2
    pub fn derive_key_pbkdf2(
        password: &[u8],
        salt: &[u8],
        iterations: u32,
        key_size: usize,
    ) -> Result<Vec<u8>> {
        use sha2::Sha256;
        use pbkdf2::pbkdf2_hmac;
        
        let mut key = vec![0u8; key_size];
        pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut key);
        Ok(key)
    }
    
    /// Derive key from password using Argon2
    pub fn derive_key_argon2(
        password: &[u8],
        salt: &[u8],
        time_cost: u32,
        memory_cost: u32,
        parallelism: u32,
        key_size: usize,
    ) -> Result<Vec<u8>> {
        use argon2::{Argon2, Params};

        let params = Params::new(memory_cost, time_cost, parallelism, None)
            .map_err(|e| BackupError::Encryption(format!("Invalid Argon2 params: {}", e)))?;

        let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);

        let mut key = vec![0u8; key_size];
        argon2
            .hash_password_into(password, salt, &mut key)
            .map_err(|e| BackupError::Encryption(format!("Argon2 error: {}", e)))?;

        Ok(key)
    }
}

/// Encryption metrics
#[derive(Debug, Clone)]
pub struct EncryptionMetrics {
    /// Total bytes encrypted
    pub bytes_encrypted: u64,
    /// Total bytes decrypted
    pub bytes_decrypted: u64,
    /// Number of encryption operations
    pub encryption_ops: u64,
    /// Number of decryption operations
    pub decryption_ops: u64,
    /// Average encryption time (microseconds)
    pub avg_encryption_time_us: u64,
    /// Average decryption time (microseconds)
    pub avg_decryption_time_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encryption_type_properties() {
        assert_eq!(EncryptionType::Aes256Gcm.key_size(), 32);
        assert_eq!(EncryptionType::Aes256Gcm.nonce_size(), 12);
        assert_eq!(EncryptionType::Aes256Gcm.tag_size(), 16);

        assert_eq!(EncryptionType::ChaCha20Poly1305.key_size(), 32);
        assert_eq!(EncryptionType::ChaCha20Poly1305.nonce_size(), 12);

        assert_eq!(EncryptionType::Aes256CtrHmacSha256.key_size(), 32);
        assert_eq!(EncryptionType::Aes256CtrHmacSha256.nonce_size(), 16);
    }

    #[test]
    fn test_memory_key_manager() {
        let manager = MemoryKeyManager::new();

        // Generate key
        manager
            .generate_key("test-key", EncryptionType::Aes256Gcm)
            .unwrap();

        // Retrieve key
        let key = manager.get_key("test-key").unwrap();
        assert_eq!(key.len(), 32);

        // List keys
        let keys = manager.list_keys().unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&"test-key".to_string()));

        // Rotate key
        let new_key_id = manager.rotate_key("test-key").unwrap();
        assert!(new_key_id.starts_with("test-key-v"));

        // Delete key
        manager.delete_key("test-key").unwrap();
        assert!(manager.get_key("test-key").is_err());
    }

    #[test]
    fn test_nonce_generation() {
        let nonce1 = utils::generate_nonce(12);
        let nonce2 = utils::generate_nonce(12);

        assert_eq!(nonce1.len(), 12);
        assert_eq!(nonce2.len(), 12);
        assert_ne!(nonce1, nonce2); // Should be different
    }

    #[test]
    fn test_aes256_gcm_roundtrip() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("aes-key", EncryptionType::Aes256Gcm)
            .unwrap();

        let config = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "aes-key".to_string(),
            kdf_params: None,
            aad: None,
        };

        let plaintext = b"Hello, World! This is a test message for AES-256-GCM encryption.";

        // Encrypt
        let ciphertext = encrypt(plaintext, &config, &key_manager).unwrap();
        assert_ne!(&ciphertext[..], plaintext); // Should be different

        // Decrypt
        let decrypted = decrypt(&ciphertext, &config, &key_manager).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_chacha20_poly1305_roundtrip() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("chacha-key", EncryptionType::ChaCha20Poly1305)
            .unwrap();

        let config = EncryptionConfig {
            encryption_type: EncryptionType::ChaCha20Poly1305,
            key_id: "chacha-key".to_string(),
            kdf_params: None,
            aad: None,
        };

        let plaintext = b"Hello, World! This is a test message for ChaCha20-Poly1305 encryption.";

        // Encrypt
        let ciphertext = encrypt(plaintext, &config, &key_manager).unwrap();
        assert_ne!(&ciphertext[..], plaintext);

        // Decrypt
        let decrypted = decrypt(&ciphertext, &config, &key_manager).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_aes256_ctr_hmac_roundtrip() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("ctr-key", EncryptionType::Aes256CtrHmacSha256)
            .unwrap();

        let config = EncryptionConfig {
            encryption_type: EncryptionType::Aes256CtrHmacSha256,
            key_id: "ctr-key".to_string(),
            kdf_params: None,
            aad: None,
        };

        let plaintext = b"Hello, World! This is a test message for AES-256-CTR-HMAC encryption.";

        // Encrypt
        let ciphertext = encrypt(plaintext, &config, &key_manager).unwrap();
        assert_ne!(&ciphertext[..], plaintext);

        // Decrypt
        let decrypted = decrypt(&ciphertext, &config, &key_manager).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_encryption_with_aad() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("aad-key", EncryptionType::Aes256Gcm)
            .unwrap();

        let aad = b"additional authenticated data";
        let config = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "aad-key".to_string(),
            kdf_params: None,
            aad: Some(aad.to_vec()),
        };

        let plaintext = b"Secret message with AAD";

        // Encrypt
        let ciphertext = encrypt(plaintext, &config, &key_manager).unwrap();

        // Decrypt with same AAD
        let decrypted = decrypt(&ciphertext, &config, &key_manager).unwrap();
        assert_eq!(&decrypted[..], plaintext);

        // Decrypt with wrong AAD should fail
        let wrong_config = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "aad-key".to_string(),
            kdf_params: None,
            aad: Some(b"wrong aad".to_vec()),
        };
        assert!(decrypt(&ciphertext, &wrong_config, &key_manager).is_err());
    }

    #[test]
    fn test_encrypt_with_key_direct() {
        let key = utils::generate_nonce(32); // 256-bit key

        let plaintext = b"Direct encryption test";

        // Test AES-256-GCM
        let envelope = encrypt_with_key(plaintext, &key, EncryptionType::Aes256Gcm, None).unwrap();
        let decrypted = decrypt_with_key(&envelope, &key, None).unwrap();
        assert_eq!(&decrypted[..], plaintext);

        // Test ChaCha20-Poly1305
        let envelope =
            encrypt_with_key(plaintext, &key, EncryptionType::ChaCha20Poly1305, None).unwrap();
        let decrypted = decrypt_with_key(&envelope, &key, None).unwrap();
        assert_eq!(&decrypted[..], plaintext);

        // Test AES-256-CTR-HMAC
        let envelope =
            encrypt_with_key(plaintext, &key, EncryptionType::Aes256CtrHmacSha256, None).unwrap();
        let decrypted = decrypt_with_key(&envelope, &key, None).unwrap();
        assert_eq!(&decrypted[..], plaintext);
    }

    #[test]
    fn test_wrong_key_fails_decryption() {
        let key1 = utils::generate_nonce(32);
        let key2 = utils::generate_nonce(32);

        let plaintext = b"Secret message";

        let envelope = encrypt_with_key(plaintext, &key1, EncryptionType::Aes256Gcm, None).unwrap();

        // Decryption with wrong key should fail
        assert!(decrypt_with_key(&envelope, &key2, None).is_err());
    }

    #[test]
    fn test_tampered_ciphertext_fails() {
        let key = utils::generate_nonce(32);
        let plaintext = b"Secret message";

        let mut envelope =
            encrypt_with_key(plaintext, &key, EncryptionType::Aes256Gcm, None).unwrap();

        // Tamper with ciphertext
        if !envelope.ciphertext.is_empty() {
            envelope.ciphertext[0] ^= 0xFF;
        }

        // Decryption should fail due to authentication
        assert!(decrypt_with_key(&envelope, &key, None).is_err());
    }

    #[test]
    fn test_large_data_encryption() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("large-key", EncryptionType::Aes256Gcm)
            .unwrap();

        let config = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "large-key".to_string(),
            kdf_params: None,
            aad: None,
        };

        // 1MB of data
        let plaintext: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();

        let ciphertext = encrypt(&plaintext, &config, &key_manager).unwrap();
        let decrypted = decrypt(&ciphertext, &config, &key_manager).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_pbkdf2_key_derivation() {
        let password = b"my-secret-password";
        let salt = utils::generate_salt(16);

        let key = utils::derive_key_pbkdf2(password, &salt, 100_000, 32).unwrap();
        assert_eq!(key.len(), 32);

        // Same password + salt = same key
        let key2 = utils::derive_key_pbkdf2(password, &salt, 100_000, 32).unwrap();
        assert_eq!(key, key2);

        // Different password = different key
        let key3 = utils::derive_key_pbkdf2(b"wrong-password", &salt, 100_000, 32).unwrap();
        assert_ne!(key, key3);
    }

    #[test]
    fn test_argon2_key_derivation() {
        let password = b"my-secret-password";
        let salt = utils::generate_salt(16);

        // Use low params for test speed (real usage would be higher)
        let key = utils::derive_key_argon2(password, &salt, 1, 1024, 1, 32).unwrap();
        assert_eq!(key.len(), 32);

        // Same password + salt = same key
        let key2 = utils::derive_key_argon2(password, &salt, 1, 1024, 1, 32).unwrap();
        assert_eq!(key, key2);
    }

    #[test]
    fn test_key_id_mismatch_fails() {
        let key_manager = MemoryKeyManager::new();
        key_manager
            .generate_key("key1", EncryptionType::Aes256Gcm)
            .unwrap();
        key_manager
            .generate_key("key2", EncryptionType::Aes256Gcm)
            .unwrap();

        let config1 = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "key1".to_string(),
            kdf_params: None,
            aad: None,
        };

        let config2 = EncryptionConfig {
            encryption_type: EncryptionType::Aes256Gcm,
            key_id: "key2".to_string(),
            kdf_params: None,
            aad: None,
        };

        let plaintext = b"Secret message";
        let ciphertext = encrypt(plaintext, &config1, &key_manager).unwrap();

        // Decryption with different key_id should fail
        let result = decrypt(&ciphertext, &config2, &key_manager);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Key ID mismatch"));
    }
}