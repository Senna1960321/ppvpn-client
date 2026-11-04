"""Acceptance of memory measurements, without opening a TUN."""

import unittest
import socket
import struct
import threading
import time
from unittest.mock import patch

import ne_mem


class DnsBurstTest(unittest.TestCase):
    def burst(self, finish):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
            server.bind(("127.0.0.1", 0))
            server.settimeout(2)

            def respond():
                queries = [server.recvfrom(4096) for _ in range(2)]
                data, peer = queries[0]
                reply = data[:2] + b"\x81\x80" + data[4:]
                server.sendto(reply, peer)
                server.sendto(reply, peer)  # duplicates are not more answers
                server.sendto(struct.pack(">HHHHHH", 99, 0x8180, 0, 0, 0, 0), peer)
                server.sendto(b"short", peer)
                if finish:
                    time.sleep(0.08)
                    data, peer = queries[1]
                    server.sendto(data[:2] + b"\x81\x80" + data[4:], peer)

            worker = threading.Thread(target=respond)
            worker.start()
            with patch.object(ne_mem, "DNS_IP", "127.0.0.1"), \
                 patch.object(ne_mem, "DNS_PORT", server.getsockname()[1]), \
                 patch.object(ne_mem, "DNS_RESPONSE_TIMEOUT", 0.2):
                answered = ne_mem.dns_burst(2, rate=1000)
            worker.join(timeout=2)
            self.assertFalse(worker.is_alive())
            return answered

    def test_delayed_response_is_counted_once(self):
        self.assertGreater(ne_mem.DNS_RESPONSE_TIMEOUT, 10)
        self.assertEqual(self.burst(True), 2)

    def test_duplicates_cannot_hide_a_missing_response(self):
        self.assertEqual(self.burst(False), 1)


def row(variant="ios", label="download_anytls", **load):
    result = {"variant": variant, "label": label, "load": {
        "bytes_sent": 1000, "bytes_received": 1000,
        "connections": 8, "failed_connections": 0, "stalled_connections": 0,
        **load,
    }}
    result["failed"] = ne_mem.failure(result)
    return result


class RequiredLoadsTest(unittest.TestCase):
    def test_ios_failure_is_checked_beside_default(self):
        failed = row(bytes_received=0)
        self.assertEqual(ne_mem.failed_loads([row("default"), failed], "default"), [failed])

    def test_no_default_still_checks_selected_variant(self):
        failed = row("mobile", bytes_received=0)
        self.assertEqual(ne_mem.failed_loads([failed], "mobile"), [failed])

    def test_successful_loads_pass(self):
        self.assertEqual(ne_mem.failed_loads([row(), row("default")], "default"), [])

    def test_cap_refusals_stay_visible_without_failing_ci(self):
        failed = row(label="conns500", bytes_received=500,
                     connections=500, failed_connections=0, completed_echoes=128)
        self.assertIsNotNone(failed["failed"])
        self.assertEqual(ne_mem.failed_loads([failed], "ios"), [])

    def test_cap_does_not_excuse_a_dead_or_stalled_tunnel(self):
        for load in ({"bytes_received": 0}, {"stalled_connections": 1},
                     {"failed_connections": 500}, {"error": "loadgen died"}):
            with self.subTest(load=load):
                failed = row(label="conns500", **load)
                self.assertEqual(ne_mem.failed_loads([failed], "ios"), [failed])

    def test_handshakes_and_a_short_echo_are_not_successful_admission(self):
        for completed in (0, 1, 127):
            failed = row(label="conns500", connections=500, failed_connections=0,
                         bytes_sent=8192000, bytes_received=1, completed_echoes=completed)
            self.assertEqual(ne_mem.failed_loads([failed], "ios"), [failed])

    def test_equal_total_bytes_do_not_hide_lost_streams(self):
        for requested in (100, 128, 500):
            failed = row(label=f"conns{requested}", connections=requested,
                         completed_echoes=1, bytes_sent=16384, bytes_received=16384)
            self.assertIsNotNone(failed["failed"])
            self.assertEqual(ne_mem.failed_loads([failed], "ios"), [failed])

    def test_all_admitted_streams_complete(self):
        for requested in (100, 128, 500):
            complete = row(label=f"conns{requested}", completed_echoes=min(requested, 128))
            self.assertEqual(ne_mem.failed_loads([complete], "ios"), [])

    def test_short_echo_at_or_below_cap_is_a_failure(self):
        for label in ("conns100", "conns128", "download_ss", "switch"):
            failed = row(label=label, bytes_received=500)
            self.assertEqual(ne_mem.failed_loads([failed], "default"), [failed])

    def test_dns_failure_is_only_tolerated_under_dhat(self):
        failed = {"variant": "ios", "label": "dns", "answered": 0, "queries": 2000}
        failed["failed"] = ne_mem.failure(failed)
        self.assertEqual(ne_mem.failed_loads([failed], "ios"), [failed])
        self.assertEqual(ne_mem.failed_loads([failed], "ios", dhat=True), [])

    def test_experimental_variants_keep_reporting_failures(self):
        failed = row("budget", bytes_received=0)
        self.assertEqual(ne_mem.failed_loads([failed], "default"), [])
        self.assertIsNotNone(failed["failed"])

    def test_an_empty_variant_selection_is_rejected_before_starting(self):
        argv = ["ne_mem.py", "--ne-mem", "unused", "--fakenode", "unused",
                "--loadgen", "unused", "--no-default"]
        with patch("sys.argv", argv), patch("ne_mem.subprocess.Popen") as popen:
            with self.assertRaises(SystemExit) as exited:
                ne_mem.main()
        self.assertEqual(exited.exception.code, 2)
        popen.assert_not_called()


if __name__ == "__main__":
    unittest.main()
