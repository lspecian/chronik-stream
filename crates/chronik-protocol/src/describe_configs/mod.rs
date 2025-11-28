//! DescribeConfigs API Handler Modules
//!
//! Refactored from monolithic `handle_describe_configs()` (214 lines, ~186 complexity)
//! and `encode_describe_configs_response()` (150 lines) into focused modules
//! with <25 complexity per function.
//!
//! ## Architecture
//!
//! ### Request Flow
//! 1. **request_parser** - Parse DescribeConfigs request body
//! 2. **resource_parser** - Parse individual resource with config keys
//! 3. **config_provider** - Fetch topic/broker configurations
//! 4. **response_encoder** - Encode version-specific response
//!
//! ### Supported Versions
//! - v0: Basic topic/broker config queries
//! - v1+: Configuration keys filtering, synonyms support
//! - v3+: Documentation support
//! - v4+: Flexible/compact encoding with tagged fields
//!
//! ### Key Features
//! - Resource types: Topic (2), Broker (4)
//! - Configuration filtering by keys
//! - Synonym support (v1+)
//! - Documentation support (v3+)
//! - Tagged fields for flexible protocol (v4+)

pub mod request_parser;
pub mod resource_parser;
pub mod config_provider;
pub mod response_encoder;

pub use request_parser::RequestParser;
pub use resource_parser::ResourceParser;
pub use config_provider::ConfigProvider;
pub use response_encoder::ResponseEncoder;
