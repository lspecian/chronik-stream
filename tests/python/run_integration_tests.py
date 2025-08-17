#!/usr/bin/env python3
"""
Integration Test Runner for Chronik Stream

This script runs various integration tests against Chronik Stream using different
Kafka client libraries to ensure comprehensive compatibility.
"""

import subprocess
import sys
import os
import time
import argparse
from typing import List, Tuple, Dict
from colorama import init, Fore, Style
from tabulate import tabulate

# Initialize colorama for cross-platform colored output
init()

# Test suites to run
TEST_SUITES = {
    'kafka-python': {
        'script': 'test_kafka_python_integration.py',
        'description': 'Comprehensive kafka-python client tests',
        'requires': ['kafka-python']
    },
    'basic': {
        'script': 'test_basic_functionality.py', 
        'description': 'Basic protocol functionality tests',
        'requires': []
    },
    'metadata': {
        'script': 'test_metadata.py',
        'description': 'Metadata API tests',
        'requires': []
    },
    'produce': {
        'script': 'test_produce.py',
        'description': 'Producer functionality tests',
        'requires': []
    },
    'real-clients': {
        'script': 'test_real_clients.py',
        'description': 'Real Kafka client compatibility tests',
        'requires': ['kafka-python', 'confluent-kafka']
    }
}


def check_chronik_running() -> bool:
    """Check if Chronik Stream is running"""
    try:
        import socket
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(2)
        result = sock.connect_ex(('localhost', 9092))
        sock.close()
        return result == 0
    except:
        return False


def check_dependencies(requires: List[str]) -> Tuple[bool, List[str]]:
    """Check if required Python packages are installed"""
    missing = []
    for package in requires:
        try:
            __import__(package.replace('-', '_'))
        except ImportError:
            missing.append(package)
    return len(missing) == 0, missing


def run_test_suite(suite_name: str, script: str) -> Tuple[bool, float, str]:
    """Run a single test suite and return results"""
    print(f"\n{Fore.CYAN}Running {suite_name} tests...{Style.RESET_ALL}")
    
    start_time = time.time()
    try:
        result = subprocess.run(
            [sys.executable, script],
            capture_output=True,
            text=True,
            cwd=os.path.dirname(os.path.abspath(__file__))
        )
        
        duration = time.time() - start_time
        
        if result.returncode == 0:
            return True, duration, "All tests passed"
        else:
            # Extract error summary from output
            lines = result.stderr.split('\n') if result.stderr else result.stdout.split('\n')
            error_summary = "Test failures detected"
            for line in lines:
                if "FAILED" in line or "Error:" in line:
                    error_summary = line.strip()
                    break
            return False, duration, error_summary
            
    except Exception as e:
        duration = time.time() - start_time
        return False, duration, f"Exception: {str(e)}"


def main():
    """Main test runner"""
    parser = argparse.ArgumentParser(description='Run Chronik Stream integration tests')
    parser.add_argument('--suite', choices=list(TEST_SUITES.keys()) + ['all'], 
                       default='all', help='Test suite to run')
    parser.add_argument('--no-deps-check', action='store_true',
                       help='Skip dependency checks')
    parser.add_argument('--verbose', '-v', action='store_true',
                       help='Show detailed test output')
    
    args = parser.parse_args()
    
    print(f"{Fore.GREEN}Chronik Stream Integration Test Runner{Style.RESET_ALL}")
    print("=" * 50)
    
    # Check if Chronik Stream is running
    if not check_chronik_running():
        print(f"{Fore.RED}✗ Chronik Stream is not running on localhost:9092{Style.RESET_ALL}")
        print("\nPlease start Chronik Stream first:")
        print("  docker-compose up -d")
        sys.exit(1)
    else:
        print(f"{Fore.GREEN}✓ Chronik Stream is running{Style.RESET_ALL}")
    
    # Determine which suites to run
    if args.suite == 'all':
        suites_to_run = TEST_SUITES.items()
    else:
        suites_to_run = [(args.suite, TEST_SUITES[args.suite])]
    
    # Check dependencies
    if not args.no_deps_check:
        print("\nChecking dependencies...")
        all_deps_ok = True
        for suite_name, suite_info in suites_to_run:
            deps_ok, missing = check_dependencies(suite_info['requires'])
            if not deps_ok:
                print(f"{Fore.YELLOW}⚠ {suite_name}: Missing dependencies: {', '.join(missing)}{Style.RESET_ALL}")
                all_deps_ok = False
        
        if not all_deps_ok:
            print(f"\n{Fore.YELLOW}Install missing dependencies with:{Style.RESET_ALL}")
            print("  pip install -r requirements.txt")
            if input("\nContinue anyway? (y/N): ").lower() != 'y':
                sys.exit(1)
    
    # Run test suites
    results = []
    total_start = time.time()
    
    for suite_name, suite_info in suites_to_run:
        # Check dependencies for this suite
        deps_ok, missing = check_dependencies(suite_info['requires'])
        if not deps_ok and not args.no_deps_check:
            results.append({
                'Suite': suite_name,
                'Status': f"{Fore.YELLOW}SKIPPED{Style.RESET_ALL}",
                'Duration': '0.0s',
                'Details': f"Missing: {', '.join(missing)}"
            })
            continue
        
        # Run the test
        success, duration, details = run_test_suite(suite_name, suite_info['script'])
        
        status = f"{Fore.GREEN}PASSED{Style.RESET_ALL}" if success else f"{Fore.RED}FAILED{Style.RESET_ALL}"
        
        results.append({
            'Suite': suite_name,
            'Status': status,
            'Duration': f"{duration:.1f}s",
            'Details': details[:50] + '...' if len(details) > 50 else details
        })
        
        if args.verbose and not success:
            print(f"{Fore.RED}Full error details:{Style.RESET_ALL}")
            print(details)
    
    total_duration = time.time() - total_start
    
    # Print summary
    print(f"\n{Fore.CYAN}Test Results Summary{Style.RESET_ALL}")
    print("=" * 80)
    
    # Use tabulate for nice formatting
    print(tabulate(results, headers='keys', tablefmt='grid'))
    
    # Calculate stats
    passed = sum(1 for r in results if 'PASSED' in r['Status'])
    failed = sum(1 for r in results if 'FAILED' in r['Status'])
    skipped = sum(1 for r in results if 'SKIPPED' in r['Status'])
    
    print(f"\nTotal duration: {total_duration:.1f}s")
    print(f"Passed: {passed}, Failed: {failed}, Skipped: {skipped}")
    
    if failed > 0:
        print(f"\n{Fore.RED}Some tests failed!{Style.RESET_ALL}")
        sys.exit(1)
    elif skipped > 0:
        print(f"\n{Fore.YELLOW}All tests passed, but some were skipped.{Style.RESET_ALL}")
        sys.exit(0)
    else:
        print(f"\n{Fore.GREEN}All tests passed!{Style.RESET_ALL}")
        sys.exit(0)


if __name__ == "__main__":
    main()