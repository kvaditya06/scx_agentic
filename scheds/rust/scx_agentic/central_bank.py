#!/usr/bin/env python3
"""
Agentic Kernel — Central Bank Daemon

A standalone service that reads telemetry from the scx_agentic scheduler
and writes monetary policy decisions back. Completely decoupled from the
scheduling hot path.

Communication:
  Reads:  /run/scx_agentic/telemetry.json  (written by scheduler)
  Writes: /run/scx_agentic/policy.json     (read by scheduler)

Modes:
  --fallback    Deterministic heuristic (default, no LLM needed)
  --llm         LLM-powered decisions (requires ANTHROPIC_API_KEY or --endpoint)

Usage:
  sudo python3 central_bank.py                     # fallback mode
  sudo python3 central_bank.py --llm               # Claude API
  sudo python3 central_bank.py --llm --endpoint URL # custom endpoint
"""

import json
import os
import sys
import time
import argparse
from pathlib import Path

TELEMETRY_PATH = "/run/scx_agentic/telemetry.json"
POLICY_PATH = "/run/scx_agentic/policy.json"
POLL_INTERVAL = 2.0  # seconds

# Demurrage bounds (must match scheduler constants)
DEMURRAGE_MIN = 0.00001   # 0.001% per ms
DEMURRAGE_MAX = 0.001     # 0.1% per ms


def read_telemetry():
    """Read the latest telemetry snapshot from the scheduler."""
    try:
        with open(TELEMETRY_PATH, "r") as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def write_policy(decision):
    """Atomically write a policy decision for the scheduler to read."""
    tmp = POLICY_PATH + ".tmp"
    with open(tmp, "w") as f:
        json.dump(decision, f, indent=2)
    os.rename(tmp, POLICY_PATH)


def fallback_heuristic(snapshot):
    """
    Deterministic policy when no LLM is available.

    Rules:
    - If reserve < 10% of supply: increase demurrage (force token recycling)
    - If reserve > 50% of supply: decrease demurrage (let processes accumulate)
    - If interactive processes are starving: redistribute 1% of reserve to them
    """
    decision = {}

    total_supply = snapshot.get("total_supply", 1)
    reserve = snapshot.get("reserve_balance", 0)
    current_rate = snapshot.get("demurrage_rate", 0.0001)

    reserve_ratio = reserve / max(total_supply, 1)

    # Adjust demurrage rate
    if reserve_ratio < 0.10:
        decision["demurrage_rate"] = min(current_rate * 1.05, DEMURRAGE_MAX)
    elif reserve_ratio > 0.50:
        decision["demurrage_rate"] = max(current_rate * 0.95, DEMURRAGE_MIN)

    # Redistribute to starved interactive processes
    starved = snapshot.get("top_starved", [])
    interactive_starved = [p for p in starved if p.get("class") == "interactive"]
    if interactive_starved:
        budget = int(reserve * 0.01)
        if budget > 0:
            decision["redistribute"] = [
                {"target_class": "interactive", "tokens": budget}
            ]

    return decision


def llm_policy(snapshot, endpoint=None, api_key=None):
    """
    LLM-powered policy decision.

    Sends the telemetry snapshot to an LLM and parses the policy response.
    Falls back to heuristic if the LLM call fails.
    """
    try:
        import anthropic
    except ImportError:
        print("  [!] anthropic package not installed. pip install anthropic", file=sys.stderr)
        return fallback_heuristic(snapshot)

    if not api_key:
        api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        print("  [!] No ANTHROPIC_API_KEY set. Falling back to heuristic.", file=sys.stderr)
        return fallback_heuristic(snapshot)

    system_prompt = """You are the Central Bank of a market-based Linux CPU scheduler.

The scheduler runs a token economy where processes bid for CPU time. You control monetary policy.

You will receive a JSON telemetry snapshot containing:
- reserve_balance: unallocated tokens in the central reserve
- total_supply: total tokens in the system (conserved invariant)
- demurrage_rate: current wealth tax rate (tokens/ms, range 0.00001 to 0.001)
- top_starved: processes with lowest token balances
- numa_nodes: per-NUMA market metrics (delta_ipc, clearing prices)
- circuit_breaker_trips_last_60s: how many times the emergency stop fired

Respond with ONLY a JSON object (no markdown, no explanation) containing any of:
- "demurrage_rate": new rate (float, 0.00001 to 0.001)
- "redistribute": [{"target_class": "interactive"|"batch"|"daemon", "tokens": int}]

Rules:
- You CANNOT mint new tokens. Redistribution comes from the reserve.
- If reserve is low (<10%), increase demurrage to recycle idle tokens.
- If interactive processes are starving, redistribute to them.
- If circuit breaker is firing frequently, the market is unstable — increase demurrage.
- If everything looks healthy, make minimal or no changes.
"""

    try:
        client = anthropic.Anthropic(api_key=api_key)
        if endpoint:
            client.base_url = endpoint

        response = client.messages.create(
            model="claude-sonnet-4-20250514",
            max_tokens=256,
            system=system_prompt,
            messages=[{
                "role": "user",
                "content": json.dumps(snapshot, indent=2)
            }],
        )

        text = response.content[0].text.strip()
        decision = json.loads(text)

        # Validate bounds
        if "demurrage_rate" in decision:
            decision["demurrage_rate"] = max(
                DEMURRAGE_MIN, min(DEMURRAGE_MAX, decision["demurrage_rate"])
            )

        return decision

    except Exception as e:
        print(f"  [!] LLM call failed: {e}. Falling back to heuristic.", file=sys.stderr)
        return fallback_heuristic(snapshot)


def main():
    parser = argparse.ArgumentParser(description="Agentic Kernel Central Bank Daemon")
    parser.add_argument("--llm", action="store_true", help="Use LLM for policy decisions")
    parser.add_argument("--endpoint", type=str, help="Custom LLM API endpoint URL")
    parser.add_argument("--api-key", type=str, help="API key (or set ANTHROPIC_API_KEY)")
    parser.add_argument("--interval", type=float, default=POLL_INTERVAL, help="Poll interval in seconds")
    args = parser.parse_args()

    mode = "LLM" if args.llm else "fallback heuristic"
    print(f"Central Bank daemon started ({mode} mode)")
    print(f"  Telemetry: {TELEMETRY_PATH}")
    print(f"  Policy:    {POLICY_PATH}")
    print(f"  Interval:  {args.interval}s")
    print()

    # Ensure directory exists
    Path("/run/scx_agentic").mkdir(parents=True, exist_ok=True)

    cycle = 0
    while True:
        time.sleep(args.interval)
        cycle += 1

        snapshot = read_telemetry()
        if snapshot is None:
            if cycle % 10 == 1:
                print("  Waiting for scheduler telemetry...")
            continue

        # Choose policy engine
        if args.llm:
            decision = llm_policy(snapshot, endpoint=args.endpoint, api_key=args.api_key)
        else:
            decision = fallback_heuristic(snapshot)

        # Write policy
        if decision:
            write_policy(decision)

        # Log summary
        reserve_pct = snapshot.get("reserve_balance", 0) / max(snapshot.get("total_supply", 1), 1) * 100
        rate = decision.get("demurrage_rate", snapshot.get("demurrage_rate", 0))
        redist = sum(r.get("tokens", 0) for r in decision.get("redistribute", []))
        trips = snapshot.get("circuit_breaker_trips_last_60s", 0)
        print(
            f"  [{cycle:>4}] reserve={reserve_pct:.1f}% "
            f"demurrage={rate:.6f} "
            f"redist={redist} "
            f"cb_trips={trips}"
        )


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nCentral Bank daemon stopped.")
