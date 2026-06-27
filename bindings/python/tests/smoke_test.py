"""Smoke test for the chronik-memory Python wheel.

Verifies the binding loads, the type surface is what we expect, and the
build path works against a live Chronik cluster when one is available.

Run:
    python3 bindings/python/tests/smoke_test.py
"""

import asyncio
import os
import sys
import time
import urllib.request

import chronik_memory


def assert_module_surface() -> None:
    """The wheel exposes a `Memory` class and a `__version__` string."""
    assert hasattr(chronik_memory, "Memory"), "missing Memory class"
    assert hasattr(chronik_memory, "__version__"), "missing __version__"
    assert chronik_memory.__version__ == "0.1.0"
    methods = {m for m in dir(chronik_memory.Memory) if not m.startswith("_")}
    expected = {"build", "init_namespace", "init_namespace_full",
                "ingest_turn", "recall", "namespace", "chronik_api"}
    missing = expected - methods
    assert not missing, f"missing methods on Memory: {missing}"
    print(f"[OK] module surface — version={chronik_memory.__version__}, "
          f"methods={sorted(methods)}")


def cluster_available() -> bool:
    """True if a Chronik unified API responds on the default port."""
    try:
        with urllib.request.urlopen("http://localhost:6094/health", timeout=2) as r:
            return r.status == 200
    except Exception:
        return False


async def run_against_live_cluster() -> None:
    ulid_seed = str(int(time.time() * 1000))
    ns = f"smoke:py-binding:{ulid_seed}"
    print(f"[..] connecting to localhost:9094 / namespace={ns!r}")
    mem = await chronik_memory.Memory.build(
        kafka="localhost:9094",
        api="http://localhost:6094",
        namespace=ns,
        request_timeout_secs=15,
    )
    print(f"[OK] build — {mem!r}")
    assert mem.namespace == ns
    assert mem.chronik_api == "http://localhost:6094"

    await mem.init_namespace()
    print("[OK] init_namespace")

    ack = await mem.ingest_turn(
        role="user",
        content="I prefer Lapa, max budget 800k euros.",
        external_id=f"smoke-{ulid_seed}-1",
    )
    print(f"[OK] ingest_turn — ack={ack}")
    assert ack["topic"].startswith("mem.raw.smoke")
    assert isinstance(ack["offset"], int)
    assert isinstance(ack["deduped"], bool)

    # Smoke recall — won't return anything because no extraction was run,
    # but it must not error.
    results = await mem.recall("budget", k=5)
    print(f"[OK] recall — {len(results)} result(s)")
    assert isinstance(results, list)


def main() -> int:
    assert_module_surface()
    if not cluster_available():
        print("[skip] no live Chronik on localhost:6094 — module-surface "
              "checks done. Start `tests/cluster/start.sh` for a live run.")
        return 0
    asyncio.run(run_against_live_cluster())
    print("\n[PASS] smoke test")
    return 0


if __name__ == "__main__":
    sys.exit(main())
