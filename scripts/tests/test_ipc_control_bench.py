"""Protocol and timing canaries for the real-process IPC benchmark."""

import contextlib
import os
import socket
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import bench_ipc_control as bench


@contextlib.contextmanager
def responder(chunks, delay=0):
    with tempfile.TemporaryDirectory(prefix="ipc-bench-") as directory:
        endpoint = os.path.join(directory, "d.sock")
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            listener.bind(endpoint)
        except OSError as error:
            listener.close()
            raise unittest.SkipTest(f"local socket binding unavailable: {error}") from error
        listener.listen(1)
        listener.settimeout(2)
        received = []
        errors = []

        def serve():
            try:
                with listener.accept()[0] as client:
                    client.settimeout(2)
                    body = bytearray()
                    while not body.endswith(b"\n"):
                        chunk = client.recv(1024)
                        if not chunk:
                            raise RuntimeError("fixture received EOF before a complete request")
                        body.extend(chunk)
                    received.append(bytes(body))
                    for chunk in chunks:
                        time.sleep(delay)
                        client.sendall(chunk)
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=serve)
        worker.start()
        try:
            yield endpoint, received
        finally:
            worker.join(timeout=3)
            listener.close()
            if worker.is_alive():
                raise AssertionError("fixture responder did not stop")
            if errors:
                raise errors[0]


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "Unix-only benchmark")
class ClientCanaries(unittest.TestCase):
    def test_fragmented_response_is_complete_and_measured_through_wait(self):
        with responder([b'{"success":', b'true,"data":{"value":7}}\n'], delay=0.01) as (endpoint, received):
            response, elapsed = bench.checked_request(endpoint, {"command": "ping"}, 1)
            self.assertEqual(response["data"]["value"], 7)
            self.assertGreaterEqual(elapsed, 10_000_000)
            self.assertEqual(received, [b'{"command":"ping"}\n'])

    def test_eof_without_delimiter_is_not_a_successful_roundtrip(self):
        with responder([b'{"success":true}']) as (endpoint, _):
            with self.assertRaisesRegex(RuntimeError, "complete response"):
                bench.request(endpoint, b'{"command":"ping"}', 1)

    def test_response_extent_is_bounded(self):
        with responder([b"x" * 33 + b"\n"]) as (endpoint, _):
            with mock.patch.object(bench, "MAX_RESPONSE_BYTES", 32):
                with self.assertRaisesRegex(RuntimeError, "safety bound"):
                    bench.request(endpoint, b'{"command":"ping"}', 1)

    def test_invalid_success_type_cannot_count_as_a_response(self):
        with responder([b'{"success":"true"}\n']) as (endpoint, _):
            with self.assertRaisesRegex(RuntimeError, "response envelope"):
                bench.request(endpoint, b'{"command":"ping"}', 1)

    def test_failed_command_cannot_count_as_successful_load(self):
        with responder([b'{"success":false,"error":"fixture failure"}\n']) as (endpoint, _):
            with self.assertRaisesRegex(RuntimeError, "fixture failure"):
                bench.checked_request(endpoint, {"command": "ping"}, 1)


if __name__ == "__main__":
    unittest.main()
