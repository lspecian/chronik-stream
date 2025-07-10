#!/usr/bin/env python3
"""Test if version comparison might have issues."""

# What if the version is stored as u16 but compared as i16?
# Or some other type issue?

def test_version_comparisons():
    # Test various version values
    versions = [0, 1, 2, 3, 4, -1, 32767, -32768]
    
    for v in versions:
        # As signed 16-bit
        as_i16 = v if -32768 <= v <= 32767 else (v & 0xFFFF) - (0x10000 if v & 0x8000 else 0)
        
        # As unsigned 16-bit  
        as_u16 = v & 0xFFFF
        
        print(f"Version {v}:")
        print(f"  as i16: {as_i16}")
        print(f"  as u16: {as_u16}")
        print(f"  >= 3 (as i16): {as_i16 >= 3}")
        print(f"  >= 3 (as u16): {as_u16 >= 3}")
        print()

test_version_comparisons()