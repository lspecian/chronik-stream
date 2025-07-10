#!/usr/bin/env python3
"""Capture and analyze Kafka protocol traffic."""

import socket
import struct
import threading
import time

def run_proxy(listen_port, target_host, target_port):
    """Run a proxy to capture traffic between kafkactl and chronik."""
    proxy = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    proxy.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    proxy.bind(('0.0.0.0', listen_port))
    proxy.listen(1)
    
    print(f"Proxy listening on port {listen_port}")
    
    client_sock, addr = proxy.accept()
    print(f"Client connected from {addr}")
    
    # Connect to target
    target_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    target_sock.connect((target_host, target_port))
    print(f"Connected to target {target_host}:{target_port}")
    
    def forward_data(src, dst, name):
        try:
            while True:
                data = src.recv(4096)
                if not data:
                    break
                    
                print(f"\n{name} ({len(data)} bytes):")
                print("Hex:", data.hex())
                
                # Try to parse as Kafka request/response
                if len(data) >= 4:
                    length = struct.unpack('>I', data[:4])[0]
                    print(f"Length field: {length}")
                    
                    if len(data) >= 8:
                        # Check if this looks like a request (has API key)
                        if name == "Client->Server":
                            if len(data) >= 10:
                                api_key = struct.unpack('>h', data[4:6])[0]
                                api_version = struct.unpack('>h', data[6:8])[0]
                                correlation_id = struct.unpack('>I', data[8:12])[0]
                                print(f"API Key: {api_key}, Version: {api_version}, Correlation ID: {correlation_id}")
                        else:
                            # Response should have correlation ID
                            correlation_id = struct.unpack('>I', data[4:8])[0]
                            print(f"Correlation ID in response: {correlation_id}")
                
                dst.send(data)
        except Exception as e:
            print(f"{name} error: {e}")
        finally:
            src.close()
            dst.close()
    
    # Start forwarding threads
    t1 = threading.Thread(target=forward_data, args=(client_sock, target_sock, "Client->Server"))
    t2 = threading.Thread(target=forward_data, args=(target_sock, client_sock, "Server->Client"))
    
    t1.start()
    t2.start()
    
    t1.join()
    t2.join()

if __name__ == "__main__":
    # Run proxy on port 9093, forwarding to chronik on 9092
    run_proxy(9093, 'localhost', 9092)