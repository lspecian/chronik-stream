#!/usr/bin/env python3
"""Fix missing api_key field in Response structs."""

import re

# Read the file
with open('crates/chronik-server/src/kafka_handler.rs', 'r') as f:
    content = f.read()

# Find all Response struct creations
pattern = r'(Ok\(Response \{[^}]+is_flexible: [^,]+,)(\s*\})'

def add_api_key(match):
    """Add api_key field to Response struct."""
    return match.group(1) + '\n                    api_key: ApiKey::ApiVersions,  // TODO: Set correct API key' + match.group(2)

# Replace all occurrences
new_content = re.sub(pattern, add_api_key, content, flags=re.MULTILINE | re.DOTALL)

# Write back
with open('crates/chronik-server/src/kafka_handler.rs', 'w') as f:
    f.write(new_content)

print("Fixed all Response structs")