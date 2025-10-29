#!/usr/bin/env python3
"""
Comprehensive ProduceHandler Flush Profile Benchmark Suite

Tests all 4 profiles (LowLatency, Balanced, HighThroughput, Extreme) across:
- Message sizes: 256B, 1KB, 4KB, 16KB
- Concurrency levels: 1, 8, 32, 64, 128, 256, 512
- Load patterns: steady, bursty, mixed

Usage:
    python3 tests/bench_all_profiles.py [--quick] [--profiles low,balanced,high,extreme]
"""

import subprocess
import time
import json
import sys
import os
import signal
import argparse
import re
from pathlib import Path
from typing import Dict, List, Optional
from datetime import datetime

# Test configurations
PROFILES = ["low-latency", "balanced", "high-throughput", "extreme"]
MESSAGE_SIZES = [256, 1024, 4096, 16384]  # bytes
CONCURRENCY_LEVELS = [1, 8, 32, 64, 128, 256, 512]
LOAD_PATTERNS = ["steady", "bursty", "mixed"]

# Quick test mode (fewer combinations)
QUICK_MESSAGE_SIZES = [1024, 4096]
QUICK_CONCURRENCY = [8, 64, 128]
QUICK_PATTERNS = ["steady"]

# Benchmark parameters
WARMUP_DURATION = "2s"
BENCHMARK_DURATION = "30s"  # Longer for more stable results
REPORT_INTERVAL = "5s"

class BenchmarkRunner:
    def __init__(self, quick_mode: bool = False, selected_profiles: Optional[List[str]] = None):
        self.quick_mode = quick_mode
        self.selected_profiles = selected_profiles or PROFILES
        self.results = []
        self.server_process = None

        # Use quick mode configurations if enabled
        self.message_sizes = QUICK_MESSAGE_SIZES if quick_mode else MESSAGE_SIZES
        self.concurrency_levels = QUICK_CONCURRENCY if quick_mode else CONCURRENCY_LEVELS
        self.load_patterns = QUICK_PATTERNS if quick_mode else LOAD_PATTERNS

        # Calculate total tests
        self.total_tests = (
            len(self.selected_profiles) *
            len(self.message_sizes) *
            len(self.concurrency_levels) *
            len(self.load_patterns)
        )
        self.current_test = 0

    def cleanup(self):
        """Kill any running chronik processes"""
        print("🧹 Cleaning up...")
        subprocess.run(["killall", "-9", "chronik-server", "chronik-bench"],
                      stderr=subprocess.DEVNULL)
        time.sleep(2)

        # Clean data directory
        subprocess.run(["rm", "-rf", "./data"], check=False)

    def start_server(self, profile: str) -> Optional[subprocess.Popen]:
        """Start chronik-server with specified profile"""
        print(f"🚀 Starting server with profile: {profile}")

        env = os.environ.copy()
        env["CHRONIK_PRODUCE_PROFILE"] = profile
        env["PATH"] = f"{os.path.expanduser('~/.cargo/bin')}:{env['PATH']}"

        log_file = f"/tmp/chronik_bench_{profile}.log"
        with open(log_file, "w") as f:
            proc = subprocess.Popen(
                ["./target/release/chronik-server",
                 "--advertised-addr", "localhost",
                 "standalone"],
                env=env,
                stdout=f,
                stderr=subprocess.STDOUT
            )

        # Wait for server to be ready
        time.sleep(5)

        # Verify server is running
        if proc.poll() is not None:
            print(f"❌ Server failed to start (profile: {profile})")
            return None

        return proc

    def run_benchmark(self,
                     profile: str,
                     message_size: int,
                     concurrency: int,
                     pattern: str) -> Dict:
        """Run a single benchmark configuration"""

        self.current_test += 1
        progress = f"[{self.current_test}/{self.total_tests}]"

        print(f"\n{'='*80}")
        print(f"{progress} Testing: profile={profile}, size={message_size}B, "
              f"concurrency={concurrency}, pattern={pattern}")
        print(f"{'='*80}")

        # Build chronik-bench command
        cmd = [
            "./target/release/chronik-bench",
            "--bootstrap-servers", "localhost:9092",
            "--topic", f"bench-{profile}-{message_size}-{concurrency}-{pattern}",
            "--mode", "produce",
            "--message-size", str(message_size),
            "--concurrency", str(concurrency),
            "--duration", BENCHMARK_DURATION,
            "--warmup-duration", WARMUP_DURATION,
            "--report-interval-secs", REPORT_INTERVAL.rstrip('s'),
        ]

        # Add pattern-specific flags
        if pattern == "bursty":
            cmd.extend(["--burst-size", "1000", "--burst-interval-ms", "100"])
        elif pattern == "mixed":
            cmd.extend(["--burst-size", "500", "--burst-interval-ms", "50"])

        env = os.environ.copy()
        env["PATH"] = f"{os.path.expanduser('~/.cargo/bin')}:{env['PATH']}"

        try:
            result = subprocess.run(
                cmd,
                env=env,
                capture_output=True,
                text=True,
                timeout=300  # 5 minute timeout
            )

            # Parse output
            output = result.stdout
            throughput = self._parse_throughput(output)
            latency = self._parse_latency(output)

            return {
                "profile": profile,
                "message_size": message_size,
                "concurrency": concurrency,
                "pattern": pattern,
                "throughput_msg_s": throughput,
                "latency_p50_ms": latency.get("p50", 0),
                "latency_p95_ms": latency.get("p95", 0),
                "latency_p99_ms": latency.get("p99", 0),
                "success": True,
                "output": output[-500:]  # Last 500 chars
            }

        except subprocess.TimeoutExpired:
            print(f"❌ Benchmark timed out")
            return {
                "profile": profile,
                "message_size": message_size,
                "concurrency": concurrency,
                "pattern": pattern,
                "success": False,
                "error": "timeout"
            }
        except Exception as e:
            print(f"❌ Benchmark failed: {e}")
            return {
                "profile": profile,
                "message_size": message_size,
                "concurrency": concurrency,
                "pattern": pattern,
                "success": False,
                "error": str(e)
            }

    def _parse_throughput(self, output: str) -> float:
        """Parse throughput from chronik-bench output

        Expected format: "║ Message rate:            3,261 msg/s"
        """
        for line in output.split('\n'):
            if 'Message rate:' in line and 'msg/s' in line:
                try:
                    # Extract: "║ Message rate:            3,261 msg/s"
                    # Split by "msg/s" and take everything before it
                    before_msg_s = line.split('msg/s')[0]
                    # Extract the number (after "Message rate:")
                    number_part = before_msg_s.split('Message rate:')[-1].strip()
                    # Remove commas and parse
                    return float(number_part.replace(',', ''))
                except Exception as e:
                    print(f"⚠️  Failed to parse throughput from line: {line}")
                    print(f"   Error: {e}")
        return 0.0

    def _parse_latency(self, output: str) -> Dict[str, float]:
        """Parse latency percentiles from chronik-bench output

        Expected format: "║ p99:                     2,885 μs  (    2.88 ms)"
        """
        latency = {}
        for line in output.split('\n'):
            # Look for p50 line
            if ' p50:' in line and 'ms)' in line:
                try:
                    # Extract: "║ p50:                     1,535 μs  (    1.53 ms)"
                    # Get the value in parentheses (ms)
                    ms_match = re.search(r'\(\s*([\d.]+)\s*ms\)', line)
                    if ms_match:
                        latency['p50'] = float(ms_match.group(1))
                except Exception as e:
                    print(f"⚠️  Failed to parse p50 from line: {line}")
                    print(f"   Error: {e}")

            # Look for p95 line
            if ' p95:' in line and 'ms)' in line:
                try:
                    ms_match = re.search(r'\(\s*([\d.]+)\s*ms\)', line)
                    if ms_match:
                        latency['p95'] = float(ms_match.group(1))
                except Exception as e:
                    print(f"⚠️  Failed to parse p95 from line: {line}")
                    print(f"   Error: {e}")

            # Look for p99 line
            if ' p99:' in line and 'ms)' in line:
                try:
                    ms_match = re.search(r'\(\s*([\d.]+)\s*ms\)', line)
                    if ms_match:
                        latency['p99'] = float(ms_match.group(1))
                except Exception as e:
                    print(f"⚠️  Failed to parse p99 from line: {line}")
                    print(f"   Error: {e}")

        return latency

    def run_all(self):
        """Run all benchmark configurations"""
        print(f"\n{'#'*80}")
        print(f"# Comprehensive ProduceHandler Flush Profile Benchmark")
        print(f"# Mode: {'QUICK' if self.quick_mode else 'FULL'}")
        print(f"# Total tests: {self.total_tests}")
        print(f"# Started: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
        print(f"{'#'*80}\n")

        for profile in self.selected_profiles:
            print(f"\n{'*'*80}")
            print(f"* TESTING PROFILE: {profile.upper()}")
            print(f"{'*'*80}\n")

            # Clean and start server for this profile
            self.cleanup()
            self.server_process = self.start_server(profile)

            if not self.server_process:
                print(f"❌ Skipping profile {profile} due to server start failure")
                continue

            # Run all combinations for this profile
            for message_size in self.message_sizes:
                for concurrency in self.concurrency_levels:
                    for pattern in self.load_patterns:
                        result = self.run_benchmark(
                            profile, message_size, concurrency, pattern
                        )
                        self.results.append(result)

                        # Show quick summary
                        if result["success"]:
                            print(f"✅ Result: {result['throughput_msg_s']:.0f} msg/s, "
                                  f"p99={result['latency_p99_ms']:.2f}ms")

                        # Brief pause between tests
                        time.sleep(2)

            # Stop server after testing this profile
            if self.server_process:
                self.server_process.terminate()
                self.server_process.wait(timeout=5)
                self.server_process = None

        # Final cleanup
        self.cleanup()

    def save_results(self, output_file: str):
        """Save results to JSON file"""
        results_data = {
            "timestamp": datetime.now().isoformat(),
            "mode": "quick" if self.quick_mode else "full",
            "total_tests": self.total_tests,
            "successful_tests": sum(1 for r in self.results if r["success"]),
            "results": self.results
        }

        with open(output_file, 'w') as f:
            json.dump(results_data, f, indent=2)

        print(f"\n📊 Results saved to: {output_file}")

    def print_summary(self):
        """Print summary of results"""
        print(f"\n{'='*80}")
        print("BENCHMARK SUMMARY")
        print(f"{'='*80}\n")

        successful = [r for r in self.results if r["success"]]

        if not successful:
            print("❌ No successful benchmarks")
            return

        print(f"Total tests: {self.total_tests}")
        print(f"Successful: {len(successful)}")
        print(f"Failed: {self.total_tests - len(successful)}\n")

        # Find best results per profile
        profiles_tested = set(r["profile"] for r in successful)

        print("🏆 BEST THROUGHPUT PER PROFILE:\n")
        for profile in profiles_tested:
            profile_results = [r for r in successful if r["profile"] == profile]
            best = max(profile_results, key=lambda x: x["throughput_msg_s"])

            print(f"{profile.upper()}:")
            print(f"  Throughput: {best['throughput_msg_s']:.0f} msg/s")
            print(f"  Latency p99: {best['latency_p99_ms']:.2f}ms")
            print(f"  Config: size={best['message_size']}B, "
                  f"concurrency={best['concurrency']}, pattern={best['pattern']}")
            print()

        # Overall winner
        best_overall = max(successful, key=lambda x: x["throughput_msg_s"])
        print(f"🥇 OVERALL BEST:")
        print(f"  Profile: {best_overall['profile']}")
        print(f"  Throughput: {best_overall['throughput_msg_s']:.0f} msg/s")
        print(f"  Latency p99: {best_overall['latency_p99_ms']:.2f}ms")
        print(f"  Config: size={best_overall['message_size']}B, "
              f"concurrency={best_overall['concurrency']}, "
              f"pattern={best_overall['pattern']}")

def main():
    parser = argparse.ArgumentParser(
        description="Comprehensive ProduceHandler Flush Profile Benchmark"
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="Run quick test mode (fewer combinations)"
    )
    parser.add_argument(
        "--profiles",
        type=str,
        help="Comma-separated list of profiles to test (e.g., 'low,high,extreme')"
    )
    parser.add_argument(
        "--output",
        type=str,
        default="/tmp/chronik_profile_benchmark_results.json",
        help="Output file for results (default: /tmp/chronik_profile_benchmark_results.json)"
    )

    args = parser.parse_args()

    # Parse profiles if specified
    selected_profiles = None
    if args.profiles:
        profile_map = {
            "low": "low-latency",
            "balanced": "balanced",
            "high": "high-throughput",
            "extreme": "extreme"
        }
        selected = []
        for p in args.profiles.split(','):
            p = p.strip().lower()
            if p in profile_map:
                selected.append(profile_map[p])
            elif p in PROFILES:
                selected.append(p)
            else:
                print(f"⚠️  Unknown profile: {p}")

        if not selected:
            print("❌ No valid profiles specified")
            return 1

        selected_profiles = selected
        print(f"Testing profiles: {', '.join(selected_profiles)}")

    # Verify binaries exist
    if not Path("./target/release/chronik-server").exists():
        print("❌ chronik-server binary not found. Run: cargo build --release --bin chronik-server")
        return 1

    if not Path("./target/release/chronik-bench").exists():
        print("❌ chronik-bench binary not found. Run: cargo build --release --bin chronik-bench")
        return 1

    # Run benchmarks
    runner = BenchmarkRunner(quick_mode=args.quick, selected_profiles=selected_profiles)

    try:
        runner.run_all()
        runner.save_results(args.output)
        runner.print_summary()
        return 0

    except KeyboardInterrupt:
        print("\n\n⚠️  Benchmark interrupted by user")
        runner.cleanup()
        return 130

    except Exception as e:
        print(f"\n\n❌ Benchmark failed: {e}")
        import traceback
        traceback.print_exc()
        runner.cleanup()
        return 1

if __name__ == "__main__":
    sys.exit(main())
