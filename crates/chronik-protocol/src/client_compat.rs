//! Client compatibility layer for handling different Kafka client implementations.
//!
//! This module provides automatic detection and adaptation for various Kafka clients,
//! handling their specific encoding preferences and protocol quirks.

use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Identifies different Kafka client types based on their software name/version
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientType {
    /// Apache Kafka Java client and Confluent clients
    JavaConfluent,
    /// KSQLDB (uses Kafka Streams/Java client internally)
    KsqlDB,
    /// librdkafka-based clients (C/C++, Python, .NET, etc.)
    LibRdKafka,
    /// Shopify Sarama (Go)
    Sarama,
    /// confluent-kafka-go
    ConfluentGo,
    /// Node.js clients using node-rdkafka
    NodeRdKafka,
    /// Generic Java client (not specifically identified)
    GenericJava,
    /// Unknown or unidentified client
    Unknown,
}

impl ClientType {
    /// Detect client type from software name and version strings
    pub fn detect(software_name: Option<&str>, software_version: Option<&str>) -> Self {
        match (software_name, software_version) {
            (Some(name), Some(version)) => {
                debug!("Detecting client type: name='{}', version='{}'", name, version);

                // Java/Confluent clients
                if name.contains("apache-kafka-java") || name.contains("kafka-java") {
                    return ClientType::JavaConfluent;
                }

                // KSQLDB uses specific version patterns
                if version.contains("-ccs") || version.contains("ksql") {
                    return ClientType::KsqlDB;
                }

                // librdkafka clients
                if name.contains("librdkafka") || name.contains("rdkafka") {
                    return ClientType::LibRdKafka;
                }

                // Sarama
                if name.contains("sarama") {
                    return ClientType::Sarama;
                }

                // confluent-kafka-go
                if name.contains("confluent") && name.contains("golang") {
                    return ClientType::ConfluentGo;
                }

                // Node clients
                if name.contains("node") && name.contains("kafka") {
                    return ClientType::NodeRdKafka;
                }

                // Generic Java detection
                if name.contains("java") {
                    return ClientType::GenericJava;
                }
            }
            (Some(name), None) => {
                // Fallback detection based on name only
                if name.contains("java") {
                    return ClientType::GenericJava;
                }
                if name.contains("rdkafka") {
                    return ClientType::LibRdKafka;
                }
            }
            _ => {}
        }

        ClientType::Unknown
    }
}

/// Encoding preference for protocol messages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingPreference {
    /// Uses flexible encoding (compact strings with varint lengths)
    Flexible,
    /// Uses non-flexible encoding (2-byte length prefixes)
    NonFlexible,
    /// Adaptive - can handle both, prefer based on context
    Adaptive,
}

/// API-specific behavior quirks
#[derive(Debug, Clone)]
pub struct ApiQuirk {
    /// Minimum version that uses flexible encoding
    pub flexible_version_threshold: i16,
    /// Whether client sends non-standard encoding for this API
    pub uses_non_standard_encoding: bool,
    /// Special handling notes
    pub notes: Option<String>,
}

/// Complete profile for a client type
#[derive(Debug, Clone)]
pub struct ClientProfile {
    /// The identified client type
    pub client_type: ClientType,
    /// Default encoding preference
    pub default_encoding: EncodingPreference,
    /// API-specific quirks (API key -> quirk)
    pub api_quirks: HashMap<i16, ApiQuirk>,
    /// Whether this client requires special KSQL compatibility
    pub requires_ksql_compat: bool,
    /// Whether to log detailed compatibility info
    pub verbose_logging: bool,
}

impl ClientProfile {
    /// Create a profile for Java/Confluent clients
    pub fn java_confluent() -> Self {
        let mut api_quirks = HashMap::new();

        // ApiVersions (18) - Java clients use non-flexible for v3+
        api_quirks.insert(18, ApiQuirk {
            flexible_version_threshold: 99, // Never uses flexible
            uses_non_standard_encoding: true,
            notes: Some("Java clients use non-flexible encoding even for v3+".to_string()),
        });

        Self {
            client_type: ClientType::JavaConfluent,
            default_encoding: EncodingPreference::NonFlexible,
            api_quirks,
            requires_ksql_compat: false,
            verbose_logging: true,
        }
    }

    /// Create a profile for KSQLDB
    pub fn ksqldb() -> Self {
        let mut profile = Self::java_confluent();
        profile.client_type = ClientType::KsqlDB;
        profile.requires_ksql_compat = true;

        // KSQLDB may have additional requirements
        profile.api_quirks.insert(3, ApiQuirk {
            flexible_version_threshold: 12,
            uses_non_standard_encoding: false,
            notes: Some("KSQLDB requires full metadata support".to_string()),
        });

        profile
    }

    /// Create a profile for librdkafka clients
    pub fn librdkafka() -> Self {
        Self {
            client_type: ClientType::LibRdKafka,
            default_encoding: EncodingPreference::Adaptive,
            api_quirks: HashMap::new(),
            requires_ksql_compat: false,
            verbose_logging: false,
        }
    }

    /// Create a profile for Sarama clients
    pub fn sarama() -> Self {
        Self {
            client_type: ClientType::Sarama,
            default_encoding: EncodingPreference::Flexible,
            api_quirks: HashMap::new(),
            requires_ksql_compat: false,
            verbose_logging: false,
        }
    }

    /// Create a default/unknown client profile
    pub fn unknown() -> Self {
        Self {
            client_type: ClientType::Unknown,
            default_encoding: EncodingPreference::Adaptive,
            api_quirks: HashMap::new(),
            requires_ksql_compat: false,
            verbose_logging: true, // Log more for unknown clients
        }
    }

    /// Get encoding preference for a specific API and version
    pub fn encoding_for_api(&self, api_key: i16, api_version: i16) -> EncodingPreference {
        if let Some(quirk) = self.api_quirks.get(&api_key) {
            if quirk.uses_non_standard_encoding {
                return EncodingPreference::NonFlexible;
            }
            if api_version >= quirk.flexible_version_threshold {
                return EncodingPreference::Flexible;
            }
        }

        // Use default encoding
        self.default_encoding
    }

    /// Check if this client requires special handling for an API
    pub fn has_quirk(&self, api_key: i16) -> bool {
        self.api_quirks.contains_key(&api_key)
    }
}

/// Registry for managing client profiles
#[derive(Debug)]
pub struct ClientRegistry {
    /// Cached profiles by client identifier
    profiles: HashMap<String, ClientProfile>,
}

impl ClientRegistry {
    /// Create a new client registry
    pub fn new() -> Self {
        Self {
            profiles: HashMap::new(),
        }
    }

    /// Get or create a client profile based on identification
    pub fn get_or_create_profile(
        &mut self,
        client_id: Option<&str>,
        software_name: Option<&str>,
        software_version: Option<&str>,
    ) -> ClientProfile {
        // Create a unique key for caching
        let cache_key = format!(
            "{}:{}:{}",
            client_id.unwrap_or(""),
            software_name.unwrap_or(""),
            software_version.unwrap_or("")
        );

        // Check cache first
        if let Some(profile) = self.profiles.get(&cache_key) {
            return profile.clone();
        }

        // Detect client type and create profile
        let client_type = ClientType::detect(software_name, software_version);
        let profile = match client_type {
            ClientType::JavaConfluent => ClientProfile::java_confluent(),
            ClientType::KsqlDB => ClientProfile::ksqldb(),
            ClientType::LibRdKafka => ClientProfile::librdkafka(),
            ClientType::Sarama => ClientProfile::sarama(),
            ClientType::ConfluentGo => ClientProfile::librdkafka(), // Similar to librdkafka
            ClientType::NodeRdKafka => ClientProfile::librdkafka(),
            ClientType::GenericJava => ClientProfile::java_confluent(),
            ClientType::Unknown => ClientProfile::unknown(),
        };

        info!(
            "Created client profile: type={:?}, encoding={:?}, client_id={:?}",
            profile.client_type, profile.default_encoding, client_id
        );

        // Cache the profile
        self.profiles.insert(cache_key, profile.clone());

        profile
    }

    /// Clear cached profiles
    pub fn clear_cache(&mut self) {
        self.profiles.clear();
    }
}

/// Helper for encoding detection and fallback
pub struct EncodingDetector;

impl EncodingDetector {
    /// Try to detect encoding by examining the buffer
    /// Returns true if likely flexible encoding, false if likely non-flexible
    pub fn detect_from_buffer(buffer: &[u8]) -> EncodingPreference {
        if buffer.is_empty() {
            return EncodingPreference::Adaptive;
        }

        // Check first byte for varint patterns
        let first_byte = buffer[0];

        // Flexible encoding uses compact strings (varint length + 1)
        // Non-flexible uses 2-byte big-endian length

        // If first byte has continuation bit set (0x80), likely varint
        if first_byte & 0x80 != 0 {
            return EncodingPreference::Flexible;
        }

        // If first byte is 0x00 and we have at least 2 bytes, check if it's a length
        if first_byte == 0x00 && buffer.len() >= 2 {
            let potential_length = u16::from_be_bytes([buffer[0], buffer[1]]) as usize;

            // Sanity check - if length is reasonable for the buffer size
            if potential_length > 0 && potential_length < 1000 && buffer.len() >= 2 + potential_length {
                return EncodingPreference::NonFlexible;
            }
        }

        // Small values (1-127) could be either varint or first byte of length
        // Default to adaptive
        EncodingPreference::Adaptive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_detection() {
        assert_eq!(
            ClientType::detect(Some("apache-kafka-java"), Some("3.5.0")),
            ClientType::JavaConfluent
        );

        assert_eq!(
            ClientType::detect(Some("apache-kafka-java"), Some("7.5.0-ccs")),
            ClientType::KsqlDB
        );

        assert_eq!(
            ClientType::detect(Some("librdkafka"), Some("1.9.0")),
            ClientType::LibRdKafka
        );

        assert_eq!(
            ClientType::detect(Some("sarama"), Some("1.34.1")),
            ClientType::Sarama
        );

        assert_eq!(
            ClientType::detect(None, None),
            ClientType::Unknown
        );
    }

    #[test]
    fn test_encoding_detection() {
        // Flexible encoding with varint
        let flexible_buf = vec![0x81, 0x01]; // Varint 129
        assert_eq!(
            EncodingDetector::detect_from_buffer(&flexible_buf),
            EncodingPreference::Flexible
        );

        // Non-flexible with 2-byte length
        let non_flexible_buf = vec![0x00, 0x0A, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(
            EncodingDetector::detect_from_buffer(&non_flexible_buf),
            EncodingPreference::NonFlexible
        );

        // Ambiguous
        let ambiguous_buf = vec![0x05];
        assert_eq!(
            EncodingDetector::detect_from_buffer(&ambiguous_buf),
            EncodingPreference::Adaptive
        );
    }

    #[test]
    fn test_client_profiles() {
        let java_profile = ClientProfile::java_confluent();
        assert_eq!(java_profile.default_encoding, EncodingPreference::NonFlexible);
        assert!(java_profile.has_quirk(18)); // ApiVersions

        let ksql_profile = ClientProfile::ksqldb();
        assert!(ksql_profile.requires_ksql_compat);

        let encoding = java_profile.encoding_for_api(18, 3);
        assert_eq!(encoding, EncodingPreference::NonFlexible);
    }

    #[test]
    fn test_client_registry() {
        let mut registry = ClientRegistry::new();

        let profile1 = registry.get_or_create_profile(
            Some("test-client"),
            Some("apache-kafka-java"),
            Some("3.5.0"),
        );
        assert_eq!(profile1.client_type, ClientType::JavaConfluent);

        // Should return cached profile
        let profile2 = registry.get_or_create_profile(
            Some("test-client"),
            Some("apache-kafka-java"),
            Some("3.5.0"),
        );
        assert_eq!(profile1.client_type, profile2.client_type);
    }
}