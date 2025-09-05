#!/usr/bin/env python3
"""Fix missing api_key fields in Response structs by inferring from context."""

import re

# Read the file
with open('crates/chronik-server/src/kafka_handler.rs', 'r') as f:
    lines = f.readlines()

# Track current API key context
current_api = None
in_match = False

for i in range(len(lines)):
    line = lines[i]
    
    # Detect which API we're handling
    if 'ApiKey::' in line and '=>' in line:
        match = re.search(r'ApiKey::(\w+)\s*=>', line)
        if match:
            current_api = match.group(1)
            in_match = True
    
    # If we find a Response creation without api_key
    if 'Ok(Response {' in line or 'return Ok(Response {' in line:
        # Look for the closing brace
        j = i + 1
        while j < len(lines) and '})' not in lines[j]:
            j += 1
        
        if j < len(lines):
            # Check if api_key is already present
            has_api_key = False
            for k in range(i, j+1):
                if 'api_key:' in lines[k]:
                    has_api_key = True
                    break
            
            if not has_api_key:
                # Add api_key before the closing brace
                api_to_use = current_api if current_api else 'ApiVersions'
                # Find the line with 'is_flexible'
                for k in range(i, j+1):
                    if 'is_flexible:' in lines[k]:
                        # Add api_key after is_flexible
                        indent = '                    '
                        lines[k] = lines[k].rstrip() + '\n' + indent + f'api_key: ApiKey::{api_to_use},\n'
                        break

# Write back
with open('crates/chronik-server/src/kafka_handler.rs', 'w') as f:
    f.writelines(lines)

print("Fixed all Response structs with appropriate API keys")