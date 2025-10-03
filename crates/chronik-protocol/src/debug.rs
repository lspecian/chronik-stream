//! Protocol debugging utilities for byte-level analysis

use std::fmt::Write as FmtWrite;
use tracing::{debug, trace};

/// Formats bytes as a hex dump with ASCII representation
pub fn hex_dump(data: &[u8], prefix: &str) -> String {
    let mut result = String::new();
    let mut offset = 0;

    while offset < data.len() {
        // Hex offset
        let _ = write!(result, "{}{:08x}  ", prefix, offset);

        // Hex bytes
        let mut ascii = String::new();
        for i in 0..16 {
            if offset + i < data.len() {
                let byte = data[offset + i];
                let _ = write!(result, "{:02x} ", byte);

                // ASCII representation
                if byte >= 0x20 && byte <= 0x7e {
                    ascii.push(byte as char);
                } else {
                    ascii.push('.');
                }
            } else {
                result.push_str("   ");
            }

            // Extra space in the middle
            if i == 7 {
                result.push(' ');
            }
        }

        // ASCII column
        result.push_str(" |");
        result.push_str(&ascii);
        result.push_str("|\n");

        offset += 16;
    }

    result
}

/// Log protocol request with full details
pub fn log_request(
    api_key: i16,
    api_name: &str,
    api_version: i16,
    correlation_id: i32,
    client_id: Option<&str>,
    data: &[u8],
) {
    debug!(
        "=== KAFKA REQUEST ===\nAPI: {} (key={}) v{}\nCorrelation ID: {}\nClient ID: {:?}\nSize: {} bytes",
        api_name, api_key, api_version, correlation_id, client_id, data.len()
    );

    if tracing::enabled!(tracing::Level::TRACE) {
        trace!("Request bytes:\n{}", hex_dump(data, "  "));
    }
}

/// Log protocol response with full details
pub fn log_response(
    api_key: i16,
    api_name: &str,
    api_version: i16,
    correlation_id: i32,
    data: &[u8],
) {
    debug!(
        "=== KAFKA RESPONSE ===\nAPI: {} (key={}) v{}\nCorrelation ID: {}\nSize: {} bytes",
        api_name, api_key, api_version, correlation_id, data.len()
    );

    if tracing::enabled!(tracing::Level::TRACE) {
        trace!("Response bytes:\n{}", hex_dump(data, "  "));
    }
}

/// Log raw bytes received from socket
pub fn log_raw_receive(data: &[u8]) {
    if tracing::enabled!(tracing::Level::TRACE) {
        trace!("=== RAW RECEIVE ({} bytes) ===\n{}", data.len(), hex_dump(data, "  "));
    }
}

/// Log raw bytes sent to socket
pub fn log_raw_send(data: &[u8]) {
    if tracing::enabled!(tracing::Level::TRACE) {
        trace!("=== RAW SEND ({} bytes) ===\n{}", data.len(), hex_dump(data, "  "));
    }
}

/// Log parsing errors with context
pub fn log_parse_error(api_name: &str, error: &str, data: &[u8], offset: usize) {
    debug!(
        "=== PARSE ERROR ===\nAPI: {}\nError: {}\nOffset: {}/{} bytes\nContext bytes:",
        api_name, error, offset, data.len()
    );

    // Show 32 bytes before and after the error point
    let start = offset.saturating_sub(32);
    let end = std::cmp::min(offset + 32, data.len());

    if start < data.len() {
        debug!("Error context:\n{}", hex_dump(&data[start..end], "  "));

        // Mark the error position
        let rel_offset = offset - start;
        let mut marker = String::new();
        for _ in 0..10 + (rel_offset * 3) {
            marker.push(' ');
        }
        marker.push_str("^^^ ERROR HERE");
        debug!("{}", marker);
    }
}

/// Compare two byte sequences and log differences
pub fn compare_bytes(expected: &[u8], actual: &[u8], context: &str) {
    if expected != actual {
        debug!("=== BYTE COMPARISON MISMATCH ===\nContext: {}", context);
        debug!("Expected {} bytes, got {} bytes", expected.len(), actual.len());

        // Find first difference
        let mut first_diff = None;
        for i in 0..std::cmp::min(expected.len(), actual.len()) {
            if expected[i] != actual[i] {
                first_diff = Some(i);
                break;
            }
        }

        if let Some(offset) = first_diff {
            debug!("First difference at offset {}", offset);

            let start = offset.saturating_sub(16);
            let end = std::cmp::min(offset + 16, std::cmp::max(expected.len(), actual.len()));

            debug!("Expected:\n{}", hex_dump(&expected[start..std::cmp::min(end, expected.len())], "  "));
            debug!("Actual:\n{}", hex_dump(&actual[start..std::cmp::min(end, actual.len())], "  "));
        }
    }
}

/// Log API version negotiation
pub fn log_api_version_negotiation(client_type: &str, supported_versions: &[(i16, i16, i16)]) {
    debug!("=== API VERSION NEGOTIATION ===\nClient type: {}", client_type);
    debug!("Supported APIs:");
    for (key, min_ver, max_ver) in supported_versions {
        debug!("  API {} : v{} - v{}", key, min_ver, max_ver);
    }
}

/// Log client detection result
pub fn log_client_detection(
    user_agent: Option<&str>,
    client_id: Option<&str>,
    detected_type: &str,
    encoding_preference: &str,
) {
    debug!(
        "=== CLIENT DETECTION ===\nUser Agent: {:?}\nClient ID: {:?}\nDetected Type: {}\nEncoding: {}",
        user_agent, client_id, detected_type, encoding_preference
    );
}