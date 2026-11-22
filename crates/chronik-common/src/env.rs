//! Reading settings from the environment.

/// A size from the environment: `4GB`, `512MB`, `64KB` or bytes. `None` when
/// unset, or (with a warning) when not a size.
pub fn bytes(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let parsed = parse_bytes(&raw);
    if parsed.is_none() {
        tracing::warn!("{name}={raw:?} is not a size (e.g. 4GB, 512MB, 1073741824); using the default");
    }
    parsed
}

/// `4GB`, `512MB`, `64KB` (any case, binary multiples) or bytes.
pub fn parse_bytes(raw: &str) -> Option<u64> {
    let v = raw.trim().to_ascii_uppercase();
    let (num, mult) = if let Some(n) = v.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = v.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = v.strip_suffix("KB") {
        (n, 1024)
    } else {
        (v.as_str(), 1)
    };
    num.trim().parse::<u64>().ok().and_then(|n| n.checked_mul(mult))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_bytes("4GB"), Some(4 << 30));
        assert_eq!(parse_bytes(" 512mb "), Some(512 << 20));
        assert_eq!(parse_bytes("64KB"), Some(64 << 10));
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("lots"), None);
    }
}
