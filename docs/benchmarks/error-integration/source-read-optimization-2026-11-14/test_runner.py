import copy
import unittest
from run import parity


class ParityTests(unittest.TestCase):
    def test_matching_selected_error_is_accepted(self):
        error = [32, "NotFound", ["nfs", 2], None]
        parity(
            {
                "baseline": {"selected_error": error},
                "final": {"selected_error": copy.deepcopy(error)},
            }
        )

    def test_selected_native_status_change_is_rejected(self):
        with self.assertRaises(AssertionError):
            parity(
                {
                    "baseline": {"selected_error": [32, "NotFound", ["nfs", 2], None]},
                    "final": {"selected_error": [32, "NotFound", ["nfs", 5], None]},
                }
            )

    def test_work_or_cleanup_change_is_rejected(self):
        for field in ["ring", "nfs", "closed_handles", "facts"]:
            with self.subTest(field=field), self.assertRaises(AssertionError):
                parity({"baseline": {field: 1}, "final": {field: 2}})


if __name__ == "__main__":
    unittest.main()
