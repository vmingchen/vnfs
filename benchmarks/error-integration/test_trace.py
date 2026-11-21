import subprocess
import unittest
from unittest.mock import Mock, patch

from trace import decode_commands, managed_capture, require_lossless_capture


class WireControls(unittest.TestCase):
    def test_pending_and_complete_in_one_tcp_frame_are_separate(self):
        complete, pending = decode_commands(
            "0x0007,0x0007\t0x00000103,0x00000000", True
        )
        self.assertEqual(dict(complete), {7: 1})
        self.assertEqual(dict(pending), {7: 1})

    def test_terminal_compound_failures_are_counted(self):
        complete, pending = decode_commands(
            "0x0005,0x0008,0x0006\t0xc0000034,0xc0000128,0xc0000128", True
        )
        self.assertEqual(dict(complete), {5: 1, 8: 1, 6: 1})
        self.assertFalse(pending)

    def test_request_commands_need_no_response_status(self):
        complete, pending = decode_commands("0x0005,0x0008,0x0006", False)
        self.assertEqual(dict(complete), {5: 1, 8: 1, 6: 1})
        self.assertFalse(pending)

    def test_missing_dissection_is_not_a_complete_capture(self):
        with self.assertRaises(AssertionError):
            decode_commands("0x0007,0x0007\t0x00000000", True)

    def test_capture_rejects_a_nonzero_exit_after_the_workload(self):
        capture = Mock(pid=1234)
        capture.poll.return_value = None
        capture.wait.return_value = 7
        with patch("trace.subprocess.run") as signal_capture:
            with self.assertRaisesRegex(RuntimeError, "tcpdump exited with status 7"):
                with managed_capture(capture):
                    pass
        signal_capture.assert_called_once_with(
            ["sudo", "-n", "kill", "-INT", "1234"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        capture.wait.assert_called_once_with(timeout=10)

    def test_capture_shutdown_does_not_mask_the_workload_failure(self):
        timeout = subprocess.TimeoutExpired(["tcpdump"], 10)
        capture = Mock()
        capture.poll.return_value = None
        capture.wait.side_effect = timeout
        failure = ValueError("workload failed")
        with patch("trace.subprocess.run"):
            with self.assertRaises(ValueError) as result:
                with managed_capture(capture):
                    raise failure
        self.assertIs(result.exception, failure)
        self.assertIn(str(timeout), failure.__notes__[0])

    def test_capture_requires_one_zero_drop_count(self):
        require_lossless_capture("capture summary\n0 packets dropped by kernel\n")
        for summary in (
            "10 packets dropped by kernel",
            "",
            "0 packets dropped by kernel\n0 packets dropped by kernel",
        ):
            with self.subTest(summary=summary), self.assertRaises(ValueError):
                require_lossless_capture(summary)


if __name__ == "__main__":
    unittest.main()
