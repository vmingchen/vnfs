"""Real-server RPC proxy for deterministic I/O outcome conformance.

Only an armed WRITE is altered. Session negotiation, OPEN, real effects and
cleanup use the actual server. Consumers arm after OPEN and before dispatch.
No retry logic lives in this fixture: every observed WRITE is recorded.
"""

import socket
import struct
import threading

SEQUENCE, PUTFH, GETATTR, GETFH, READ, WRITE = 53, 22, 9, 10, 25, 38
UNCACHED, NOSPC = 10068, 28


def word(data, offset):
    return struct.unpack_from(">I", data, offset)[0]


def words(*values):
    return struct.pack(">" + "I" * len(values), *values)


def padded(size):
    return (size + 3) & ~3


def receive(stream):
    def exact(size):
        data = bytearray()
        while len(data) < size:
            part = stream.recv(size - len(data))
            if not part:
                raise EOFError
            data.extend(part)
        return bytes(data)

    data = bytearray()
    while True:
        marker = word(exact(4), 0)
        size = marker & 0x7FFFFFFF
        if size > 32 * 1024 * 1024 or len(data) + size > 32 * 1024 * 1024:
            raise ValueError("RPC record exceeds fixture budget")
        data.extend(exact(size))
        if marker >> 31:
            return bytes(data)


def send(stream, data):
    stream.sendall(words(0x80000000 | len(data)) + data)


def operation_offset(request):
    at = 24
    for _ in range(2):
        at += 8 + padded(word(request, at + 4))
    return at + 4 + padded(word(request, at)) + 8


def operations(request):
    at = operation_offset(request)
    result = []
    while at < len(request):
        start = at
        code = word(request, at)
        if code == SEQUENCE:
            at += 36
        elif code == PUTFH:
            at += 8 + padded(word(request, at + 4))
        elif code == GETATTR:
            at += 8 + 4 * word(request, at + 4)
        elif code == GETFH:
            at += 4
        elif code == READ:
            at += 32
        elif code == WRITE:
            at += 36 + padded(word(request, at + 32))
        else:
            # Setup/cleanup compounds contain other operations; never modify
            # those. Armed test I/O must contain only the supported operations.
            break
        if at > len(request):
            raise ValueError("truncated operation")
        result.append((code, start, at))
    return result


def shorten(request, limit):
    result = bytearray()
    prior = 0
    shortened = False
    for code, start, end in operations(request):
        if code == WRITE and not shortened:
            size = min(limit, word(request, start + 32))
            result.extend(request[prior : start + 32])
            result.extend(words(size))
            result.extend(request[start + 36 : start + 36 + size])
            result.extend(bytes(padded(size) - size))
            prior = end
            shortened = True
    result.extend(request[prior:])
    assert shortened
    return bytes(result)


def failure_reply(request, status):
    ops = operations(request)
    assert ops[0][0] == SEQUENCE
    sequence = request[ops[0][1] + 4 : ops[0][1] + 28]
    # Stop at the first PUTFH for uncached replies, at WRITE for a definite
    # no-space result. All preceding SEQUENCE/PUTFH results are successful.
    result = words(SEQUENCE, 0) + sequence + words(0, 0, 0)
    count = 1
    for code, _, _ in ops[1:]:
        count += 1
        if (status == UNCACHED and code == PUTFH) or code == WRITE:
            result += words(code, status)
            break
        assert code == PUTFH, code
        result += words(code, 0)
    else:
        raise AssertionError("no failure target")
    return request[:4] + words(1, 0, 0, 0, 0, status, 0, count) + result


SCENARIO_OUTCOMES = {
    "uncached": [("unknown", 0), ("not_attempted", 0)],
    "lost": [("unknown", 0), ("not_attempted", 0)],
    "short": [("success", 16), ("not_attempted", 0)],
    "prefix": [("failed", 4), ("not_attempted", 0)],
    "later": [("unknown", 0), ("not_attempted", 0), ("not_attempted", 0)],
    "uncached-later": [("unknown", 0), ("success", 1), ("not_attempted", 0)],
}


def fault_action(mode, wave):
    if wave == 1:
        return {
            "lost": "lost",
            "later": "lost",
            "uncached": "uncached",
            "uncached-later": "uncached",
            "short": "short",
            "prefix": "short",
        }[mode]
    return "failed" if mode == "prefix" and wave == 2 else None


class NfsOutcomeProxy:
    def __init__(self, upstream):
        self.upstream = upstream
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.listener.settimeout(0.2)
        self.endpoint = "%s:%s" % self.listener.getsockname()
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.sockets = []
        self.workers = []
        self.mode = None
        self.writes = []
        self.errors = []
        self.thread = threading.Thread(target=self._accept)
        self.thread.start()

    def arm(self, mode):
        assert mode in (
            "uncached",
            "lost",
            "prefix",
            "short",
            "later",
            "uncached-later",
        )
        with self.lock:
            assert self.mode is None
            self.mode = mode

    def _accept(self):
        while not self.stop.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            worker = threading.Thread(target=self._serve, args=(client,))
            self.workers.append(worker)
            worker.start()

    def _serve(self, client):
        try:
            with client, socket.create_connection(self.upstream, timeout=10) as server:
                client.settimeout(20)
                server.settimeout(20)
                with self.lock:
                    self.sockets.extend((client, server))
                while not self.stop.is_set():
                    request = receive(client)
                    write_ops = [op for op in operations(request) if op[0] == WRITE]
                    action = None
                    with self.lock:
                        if self.mode and write_ops:
                            self.writes.append(
                                [
                                    {
                                        "offset": struct.unpack_from(
                                            ">Q", request, start + 20
                                        )[0],
                                        "bytes": word(request, start + 32),
                                    }
                                    for _, start, _ in write_ops
                                ]
                            )
                            wave = len(self.writes)
                            action = fault_action(self.mode, wave)
                    forwarded = (
                        shorten(request, 4 if action == "short" else 0)
                        if action in ("short", "failed")
                        else request
                    )
                    send(server, forwarded)
                    reply = receive(server)
                    if action == "lost":
                        # The server executed it; the caller cannot know that.
                        return
                    if action in ("uncached", "failed"):
                        reply = failure_reply(
                            request, UNCACHED if action == "uncached" else NOSPC
                        )
                    send(client, reply)
        except (EOFError, ConnectionResetError, BrokenPipeError):
            pass
        except Exception as error:
            if not self.stop.is_set():
                self.errors.append(repr(error))

    def close(self):
        self.stop.set()
        self.listener.close()
        with self.lock:
            for stream in self.sockets:
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        self.thread.join(5)
        for worker in self.workers:
            worker.join(5)
            assert not worker.is_alive(), "proxy worker did not exit"
        assert not self.thread.is_alive()
        assert not self.errors, self.errors
