import importlib.util
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).parents[1]
SPEC = importlib.util.spec_from_file_location("promote", ROOT / "scripts" / "promote.py")
promote = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(promote)


class FinalPromotionTests(unittest.TestCase):
    def setUp(self):
        self.policy = json.loads((ROOT / "config" / "promotion-policy.json").read_text())
        self.evidence = json.loads((ROOT / "examples" / "promotion-evidence.json").read_text())

    def test_complete_evidence_passes(self):
        self.assertEqual(promote.evaluate(self.policy, self.evidence), [])

    def test_failed_fault_blocks_release(self):
        self.evidence["faults"]["slow_rpc"] = "failed"
        self.assertTrue(promote.evaluate(self.policy, self.evidence))

    def test_canary_ceiling_blocks_release(self):
        self.evidence["canary"]["funding_xlm"] = 101
        failures = promote.evaluate(self.policy, self.evidence)
        self.assertTrue(any("funding_xlm" in item for item in failures))

    def test_unsigned_manifest_blocks_release(self):
        self.evidence["signed_manifest_verified"] = False
        self.assertTrue(promote.evaluate(self.policy, self.evidence))


if __name__ == "__main__":
    unittest.main()
