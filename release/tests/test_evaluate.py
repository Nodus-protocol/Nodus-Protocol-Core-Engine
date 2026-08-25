import importlib.util
import unittest
from pathlib import Path

MODULE = Path(__file__).parents[1] / "scripts" / "evaluate.py"
SPEC = importlib.util.spec_from_file_location("evaluate", MODULE)
evaluate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evaluate)


class PromotionGateTests(unittest.TestCase):
    def setUp(self):
        self.policy = {
            "objectives": {
                "http_error_rate_max": 0.01, "duplicate_submission_count_max": 0,
                "lost_terminal_result_count_max": 0, "queue_depth_max": 10,
                "backpressure_rejection_rate_max": 0.05, "recovery_seconds_max": 120,
                "latency_p95_ms_max": 750, "latency_p99_ms_max": 1500,
            },
            "evidence": {
                "minimum_sustainable_rate": 100, "require_saturation_point": True,
                "require_provider_budget": True, "require_infrastructure_assumptions": True,
            },
        }
        self.results = {"metrics": {
            "http_req_failed": {"values": {"rate": 0}},
            "http_req_duration": {"values": {"p(95)": 100, "p(99)": 200}},
            "queue_depth_samples": {"values": {"count": 1}},
        }}
        self.capacity = {
            "sustainable_rate_rps": 120, "saturation_point_rps": 250,
            "provider_budget": {"rpc_requests_per_second": 500, "estimated_requests_per_second": 240},
            "infrastructure_assumptions": {"replicas": 3},
        }

    def test_complete_evidence_passes(self):
        self.assertEqual(evaluate.evaluate(self.policy, self.results, self.capacity), [])

    def test_safety_signal_blocks_promotion(self):
        self.results["metrics"]["duplicate_submissions"] = {"values": {"count": 1}}
        failures = evaluate.evaluate(self.policy, self.results, self.capacity)
        self.assertTrue(any("duplicate submissions" in item for item in failures))

    def test_missing_capacity_evidence_blocks_promotion(self):
        del self.capacity["provider_budget"]
        failures = evaluate.evaluate(self.policy, self.results, self.capacity)
        self.assertTrue(any("provider_budget" in item for item in failures))

    def test_provider_over_budget_blocks_promotion(self):
        self.capacity["provider_budget"]["estimated_requests_per_second"] = 501
        failures = evaluate.evaluate(self.policy, self.results, self.capacity)
        self.assertTrue(any("provider budget" in item for item in failures))


if __name__ == "__main__":
    unittest.main()
