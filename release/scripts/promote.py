#!/usr/bin/env python3
"""Combine capacity, fault, canary, and manifest evidence into one decision."""

import argparse
import json
import sys
from pathlib import Path

REQUIRED_FAULTS = {
    "redis_interruption", "database_interruption", "process_termination",
    "slow_rpc", "divergent_rpc", "rate_limiting", "packet_loss",
    "accepted_submission_timeout",
}


def evaluate(policy, evidence):
    failures = []
    if not evidence.get("capacity_gate_passed"):
        failures.append("capacity gate did not pass")
    if not evidence.get("signed_manifest_verified"):
        failures.append("signed release manifest was not verified")

    faults = evidence.get("faults", {})
    for name in sorted(REQUIRED_FAULTS):
        if faults.get(name) != "passed":
            failures.append(f"fault scenario did not pass: {name}")

    canary = evidence.get("canary", {})
    limits = policy["canary"]
    objectives = policy["objectives"]
    maxima = {
        "traffic_percent": limits["traffic_percent_max"],
        "requests_per_minute": limits["requests_per_minute_max"],
        "request_count": limits["request_count_max"],
        "funding_xlm": limits["funding_xlm_max"],
        "fees_xlm": limits["fees_xlm_max"],
        "duplicate_submissions": objectives["duplicate_submission_count_max"],
        "lost_terminal_results": objectives["lost_terminal_result_count_max"],
        "queue_depth": objectives["queue_depth_max"],
        "error_rate": objectives["http_error_rate_max"],
        "latency_p95_ms": objectives["latency_p95_ms_max"],
        "recovery_seconds": objectives["recovery_seconds_max"],
    }
    for name, maximum in maxima.items():
        actual = canary.get(name)
        if actual is None or actual > maximum:
            failures.append(f"canary {name}: {actual!r} exceeds {maximum!r}")
    if not canary.get("rollback_rehearsed"):
        failures.append("canary rollback was not rehearsed")
    return failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    args = parser.parse_args()
    failures = evaluate(
        json.loads(args.policy.read_text()), json.loads(args.evidence.read_text())
    )
    if failures:
        print("release promotion blocked:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("all release promotion gates passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
