# Chronik Stream v0.7.2 Release Notes

## Critical Bug Fix: Port Duplication in Advertised Address

This release fixes a **critical regression** introduced in v0.7.1 where the advertised address would incorrectly duplicate the port number, preventing all Kafka clients from connecting.

## The Bug (v0.7.1)

When users set `CHRONIK_ADVERTISED_ADDR=localhost:9092`, the server would advertise `localhost:9092:9092`, causing connection failures:

```yaml
# v0.7.1 Bug Example
CHRONIK_ADVERTISED_ADDR: "localhost:9092"
# Server advertised: localhost:9092:9092 ❌ (WRONG!)
```

## The Fix (v0.7.2)

The server now correctly parses address formats and handles ports properly:

```yaml
# v0.7.2 Fix
CHRONIK_ADVERTISED_ADDR: "localhost:9092"  
# Server advertises: localhost:9092 ✅ (CORRECT!)

CHRONIK_ADVERTISED_ADDR: "localhost"
# Server advertises: localhost:9092 ✅ (CORRECT!)
```

## Verified Configurations

All the following formats now work correctly:

| Configuration | Advertised Address |
|--------------|-------------------|
| `localhost` | `localhost:9092` |
| `localhost:9092` | `localhost:9092` |
| `chronik-stream` | `chronik-stream:9092` |
| `chronik-stream:9092` | `chronik-stream:9092` |
| `kafka.example.com` | `kafka.example.com:9092` |
| `kafka.example.com:29092` | `kafka.example.com:29092` |
| `[::1]` | `[::1]:9092` |
| `[::1]:9092` | `[::1]:9092` |

## Docker Configuration

For Docker deployments, you can now use either format:

```yaml
# Option 1: Hostname only (port defaults to 9092)
environment:
  CHRONIK_ADVERTISED_ADDR: "chronik-stream"

# Option 2: Hostname with port
environment:
  CHRONIK_ADVERTISED_ADDR: "chronik-stream:9092"
```

Both configurations work correctly in v0.7.2.

## Testing

The fix has been verified with:
- ✅ Python kafka-python client
- ✅ KafkaAdminClient connections
- ✅ Multiple address format configurations
- ✅ IPv6 address support

Test script included: `test_v0.7.2_fix.py`

## Migration from v0.7.1

Simply upgrade to v0.7.2. No configuration changes required. The server will now correctly handle your existing `CHRONIK_ADVERTISED_ADDR` settings.

## Installation

### Docker
```bash
docker pull ghcr.io/lspecian/chronik-stream:0.7.2
```

### Binary
Download from [GitHub Releases](https://github.com/lspecian/chronik-stream/releases/tag/v0.7.2)

## Acknowledgments

Thank you to the community member who provided detailed bug reports and test cases for both v0.7.0 and v0.7.1, helping us quickly identify and fix this critical issue.

---
*This hotfix release resolves the port duplication bug introduced in v0.7.1*