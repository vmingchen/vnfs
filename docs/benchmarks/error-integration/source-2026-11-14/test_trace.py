import unittest
from trace import decode_commands


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


if __name__ == "__main__":
    unittest.main()
