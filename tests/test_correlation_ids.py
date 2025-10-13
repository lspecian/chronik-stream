#!/usr/bin/env python3
"""Quick correlation ID test for v1.3.60 verification."""
import socket, struct

def test():
    sock = socket.socket()
    sock.connect(('localhost', 9092))

    # Send 3 pipelined Metadata requests
    for corr_id in [10, 20, 30]:
        req = struct.pack('>h', 3) + struct.pack('>h', 12) + struct.pack('>i', corr_id)
        req += b'\x0ctest-client\x00\x00\x00\x00'
        sock.sendall(struct.pack('>i', len(req)) + req)

    # Read responses
    received = []
    for _ in range(3):
        size = struct.unpack('>i', sock.recv(4))[0]
        corr_id = struct.unpack('>i', sock.recv(4))[0]
        received.append(corr_id)
        sock.recv(size - 4)  # consume rest

    sock.close()

    expected = [10, 20, 30]
    print(f"Sent:     {expected}")
    print(f"Received: {received}")

    if received == expected:
        print("✅ PASS - Correlation IDs in correct order!")
        return True
    else:
        print("❌ FAIL - Out of order!")
        return False

if __name__ == '__main__':
    import sys
    sys.exit(0 if test() else 1)
