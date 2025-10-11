#!/usr/bin/env python3
"""Debug script to examine WAL file structure."""

import struct
import sys

def read_wal_file(filepath):
    """Read and parse WAL V2 file."""
    with open(filepath, 'rb') as f:
        data = f.read()

    print(f"Total file size: {len(data)} bytes\n")

    cursor = 0
    record_num = 0

    while cursor < len(data):
        if cursor + 12 > len(data):
            print(f"End of file (not enough data for header)")
            break

        record_num += 1
        print(f"{'='*60}")
        print(f"Record #{record_num} at offset {cursor}")
        print(f"{'='*60}")

        # Read V2 header: magic(2) + version(1) + flags(1) + length(4) + crc32(4)
        magic = struct.unpack('<H', data[cursor:cursor+2])[0]
        version = data[cursor+2]
        flags = data[cursor+3]
        length = struct.unpack('<I', data[cursor+4:cursor+8])[0]
        crc32 = struct.unpack('<I', data[cursor+8:cursor+12])[0]

        print(f"Magic: 0x{magic:04x} (expect 0x7e7e)")
        print(f"Version: {version}")
        print(f"Flags: {flags}")
        print(f"Length: {length} bytes")
        print(f"CRC32: 0x{crc32:08x}")

        total_size = 12 + length  # header + payload
        if cursor + total_size > len(data):
            print(f"ERROR: Record extends beyond file (need {total_size}, have {len(data) - cursor})")
            break

        print(f"Total record size: {total_size} bytes")
        print()

        cursor += total_size

    print(f"\n{'='*60}")
    print(f"Total records in file: {record_num}")
    print(f"{'='*60}")

if __name__ == '__main__':
    if len(sys.argv) != 2:
        print("Usage: python debug_wal.py <wal_file>")
        sys.exit(1)

    read_wal_file(sys.argv[1])
