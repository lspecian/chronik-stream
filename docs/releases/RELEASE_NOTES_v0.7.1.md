# Chronik Stream v0.7.1 Release Notes

## Critical Docker Configuration Improvements

This hotfix release addresses configuration issues reported by users running Chronik Stream in Docker environments. The server now provides smart defaults and clearer guidance for advertised address configuration.

## What's Fixed

### 🔧 Smart Advertised Address Defaults
- Server now automatically detects hostname when binding to `0.0.0.0`
- Uses `HOSTNAME` environment variable (automatically set by Docker)
- Provides clear warnings and fallback to `localhost` when needed
- Prevents silent failures from advertising unresolvable `0.0.0.0` addresses

### 🐛 Configuration Parsing Improvements
- Correctly handles `CHRONIK_BIND_ADDR` with port specification (e.g., `0.0.0.0:9092`)
- All server modes now properly handle advertised address configuration
- Enhanced logging to guide users on correct configuration

### 📚 Documentation Updates
- Added prominent Docker configuration warning in README
- All examples now include required `CHRONIK_ADVERTISED_ADDR` setting
- Clear guidance on when and how to set advertised addresses

## Migration Guide

### For Docker Users

**IMPORTANT**: You must set `CHRONIK_ADVERTISED_ADDR` when running in Docker:

```yaml
# docker-compose.yml
services:
  chronik-stream:
    image: ghcr.io/lspecian/chronik-stream:0.7.1
    ports:
      - "9092:9092"
    environment:
      CHRONIK_BIND_ADDR: "0.0.0.0"  # Host only, no port
      CHRONIK_ADVERTISED_ADDR: "chronik-stream"  # REQUIRED for Docker
```

### Configuration Examples

#### Container-to-Container Communication
```yaml
CHRONIK_ADVERTISED_ADDR: "chronik-stream"  # Container hostname
```

#### Host Machine Access
```yaml
CHRONIK_ADVERTISED_ADDR: "localhost"
```

#### Remote/Production Access
```yaml
CHRONIK_ADVERTISED_ADDR: "kafka.yourcompany.com"  # Public DNS/IP
```

## Verification

After upgrading, verify your configuration:

1. Check server logs for advertised address:
   ```
   INFO Advertised: your-hostname:9092  # Good ✅
   WARN Advertised: 0.0.0.0:9092       # Bad ❌
   ```

2. Test client connectivity:
   ```python
   from kafka import KafkaProducer
   producer = KafkaProducer(
       bootstrap_servers=['your-hostname:9092'],
       api_version=(0, 10, 0)
   )
   ```

## Breaking Changes

None - this release is fully backward compatible with v0.7.0.

## Installation

### Docker
```bash
docker pull ghcr.io/lspecian/chronik-stream:0.7.1
```

### Binary
Download from [GitHub Releases](https://github.com/lspecian/chronik-stream/releases/tag/v0.7.1)

## Support

For issues or questions:
- GitHub Issues: https://github.com/lspecian/chronik-stream/issues
- Documentation: https://github.com/lspecian/chronik-stream/blob/main/README.md

---
*This hotfix release addresses critical configuration issues for Docker deployments reported in v0.7.0*