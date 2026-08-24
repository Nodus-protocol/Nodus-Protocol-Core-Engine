#!/usr/bin/env python3
"""Fail closed when release evidence does not satisfy promotion policy."""

import argparse
import json
import sys
from pathlib import Path


def metric_value(metrics, name, field, default=None):
    return metrics.get(name, {}).get("values", {}).get(field, default)


def evaluate(policy, results, capacity):
    objectives = policy["objectives"]
    metrics = results.get("metrics", {})
    failures = []
    checks = {
        "http error rate": (metric_value(metrics, "http_req_failed", "rate"), objectives["http_error_rate_max"]),
        "duplicate submissions": (metric_value(metrics, "duplicate_submissions", "count", 0), objectives["duplicate_submission_count_max"]),
        "lost terminal results": (metric_value(metrics, "lost_terminal_results", "count", 0), objectives["lost_terminal_result_count_max"]),
        "queue depth": (metric_value(metrics, "queue_depth", "max", 0), objectives["queue_depth_max"]),
        "backpressure rejection rate": (metric_value(metrics, "safe_backpressure", "rate", 0), objectives["backpressure_rejection_rate_max"]),
        "recovery seconds": (capacity.get("recovery_seconds", 0), objectives["recovery_seconds_max"]),
        "latency p95": (metric_value(metrics, "http_req_duration", "p(95)"), objectives["latency_p95_ms_max"]),
        "latency p99": (metric_value(metrics, "http_req_duration", "p(99)"), objectives["latency_p99_ms_max"]),
    }
    for name, (actual, maximum) in checks.items():
        if actual is None or actual > maximum:
            failures.append(f"{name}: {actual!r} exceeds {maximum!r}")
    if metric_value(metrics, "queue_depth_samples", "count", 0) < 1:
        failures.append("queue depth telemetry was not observed")

    evidence = policy["evidence"]
    if capacity.get("sustainable_rate_rps", 0) < evidence["minimum_sustainable_rate"]:
        failures.append("sustainable rate is below the promotion minimum")
    for required, key in (
        (evidence["require_saturation_point"], "saturation_point_rps"),
        (evidence["require_provider_budget"], "provider_budget"),
        (evidence["require_infrastructure_assumptions"], "infrastructure_assumptions"),
    ):
        if required and not capacity.get(key):
            failures.append(f"capacity report is missing {key}")
    budget = capacity.get("provider_budget", {})
    if budget.get("estimated_requests_per_second", float("inf")) > budget.get("rpc_requests_per_second", 0):
        failures.append("sustainable rate exceeds provider budget")
    return failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", required=True, type=Path)
    parser.add_argument("--results", required=True, type=Path)
    parser.add_argument("--capacity", required=True, type=Path)
    args = parser.parse_args()
    policy = json.loads(args.policy.read_text())
    results = json.loads(args.results.read_text())
    capacity = json.loads(args.capacity.read_text())
    failures = evaluate(policy, results, capacity)
    if failures:
        print("release promotion blocked:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("release promotion evidence passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
