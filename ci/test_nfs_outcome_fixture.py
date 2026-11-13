"""Server-independent checks of the shared fault fixture's wire transformations."""

import socket
import unittest

from nfs_outcome_fixture import (
    GETFH,
    NOSPC,
    PUTFH,
    SEQUENCE,
    UNCACHED,
    WRITE,
    fault_action,
    failure_reply,
    operation_offset,
    operations,
    receive,
    shorten,
    word,
    words,
)


def request():
    def write(offset, data):
        return words(WRITE) + bytes(16) + words(0, offset, 2, len(data)) + data

    # Nonempty COMPOUND tag and credentials test alignment at both envelopes.
    header = words(42, 0, 2, 100003, 4, 1, 1, 4) + b"auth" + words(0, 0)
    sequence = words(SEQUENCE) + bytes(range(16)) + words(7, 0, 0, 1)
    putfh = words(PUTFH, 4) + b"file"
    return (
        header
        + words(1)
        + b"t\0\0\0"
        + words(1, 5)
        + sequence
        + putfh
        + write(0, b"abcdefgh")
        + putfh
        + write(8, b"ijklmnop")
    )


class WireFixtureTests(unittest.TestCase):
    def test_transport_stops_and_uncached_continuation_use_distinct_actions(self):
        for mode, expected in {
            "lost": ["lost", None, None],
            "later": ["lost", None, None],
            "uncached": ["uncached", None, None],
            "uncached-later": ["uncached", None, None],
            "short": ["short", None, None],
            "prefix": ["short", "failed", None],
        }.items():
            with self.subTest(mode=mode):
                self.assertEqual([fault_action(mode, n) for n in (1, 2, 3)], expected)

    def test_shortening_preserves_auth_sequence_and_other_writes(self):
        original = request()
        for limit, expected in [(0, 0), (3, 3), (100, 8)]:
            with self.subTest(limit=limit):
                changed = shorten(original, limit)
                before, after = operations(original), operations(changed)
                self.assertEqual(
                    [op[0] for op in after], [SEQUENCE, PUTFH, WRITE, PUTFH, WRITE]
                )
                self.assertEqual(original[: before[2][1]], changed[: after[2][1]])
                self.assertEqual(word(changed, after[2][1] + 32), expected)
                self.assertEqual(
                    changed[after[2][1] + 36 : after[2][1] + 36 + expected],
                    b"abcdefgh"[:expected],
                )
                self.assertEqual(original[before[3][1] :], changed[after[3][1] :])
                self.assertEqual(word(changed, operation_offset(changed) - 4), 5)

    def test_failure_replies_keep_xid_and_sequence_identity(self):
        original = request()
        for status, count, opcode in [(UNCACHED, 2, PUTFH), (NOSPC, 3, WRITE)]:
            with self.subTest(status=status):
                reply = failure_reply(original, status)
                self.assertEqual(word(reply, 0), 42)
                self.assertEqual(reply[4:24], words(1, 0, 0, 0, 0))
                self.assertEqual(reply[24:36], words(status, 0, count))
                start = operations(original)[0][1]
                self.assertEqual(reply[44:68], original[start + 4 : start + 28])
                self.assertEqual(reply[-8:], words(opcode, status))

    def test_fragmented_records_are_reassembled(self):
        sender, receiver = socket.socketpair()
        with sender, receiver:
            sender.sendall(words(2) + b"ab" + words(0x80000001) + b"c")
            self.assertEqual(receive(receiver), b"abc")

    def test_oversized_and_truncated_records_fail_closed(self):
        sender, receiver = socket.socketpair()
        with sender, receiver:
            sender.sendall(words(0xFFFFFFFF))
            with self.assertRaises(ValueError):
                receive(receiver)
        sender, receiver = socket.socketpair()
        with sender, receiver:
            sender.sendall(words(0x80000004) + b"ab")
            sender.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                receive(receiver)

    def test_unsupported_operations_are_not_rewritten(self):
        original = request()
        at = operations(original)[2][1]
        changed = original[:at] + words(GETFH, 999) + original[at:]
        with self.assertRaises(AssertionError):
            shorten(changed, 4)


if __name__ == "__main__":
    unittest.main()
