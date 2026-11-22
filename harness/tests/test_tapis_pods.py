import importlib.util
import json
import subprocess
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Event, Thread
import unittest
from unittest.mock import ANY, call, patch


source = Path(__file__).resolve().parents[2] / "scripts/tapis-pod.py"
spec = importlib.util.spec_from_file_location("tapis_pod", source)
pods = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pods)


class PodCleanupTests(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.state = Path(directory.name)
        self.record = {"id": "eur0123", "description": "my attempt"}
        (self.state / "pod.json").write_text(json.dumps(self.record))
        (self.state / "identity").touch()

    def test_delete_uses_ownership_and_confirms_absence_before_erasing_key(self):
        with patch.object(pods, "api", side_effect=[self.record, {}, None]) as api:
            pods.delete(self.state)
        self.assertEqual(api.call_args_list, [call("GET", "/eur0123", deadline=ANY),
                                              call("DELETE", "/eur0123", deadline=ANY),
                                              call("GET", "/eur0123", deadline=ANY)])
        self.assertTrue((self.state / "deleted").is_file())
        self.assertFalse((self.state / "identity").exists())

    def test_never_deletes_a_different_operators_pod(self):
        with patch.object(pods, "api", return_value={"description": "another attempt"}) as api:
            with self.assertRaisesRegex(RuntimeError, "another attempt"):
                pods.delete(self.state)
        api.assert_called_once_with("GET", "/eur0123", deadline=ANY)
        self.assertFalse((self.state / "deleted").exists())
        self.assertTrue((self.state / "identity").is_file())

    def test_delete_recovers_a_lost_response_without_reissuing_delete(self):
        with patch.object(pods, "api", side_effect=[self.record, pods.TransientError(), self.record, None]) as api, patch.object(pods, "backoff"):
            pods.delete(self.state)
            pods.delete(self.state)
        self.assertEqual([c.args[0] for c in api.call_args_list], ["GET", "DELETE", "GET", "GET"])
        self.assertTrue((self.state / "deleted").is_file())

    def test_late_create_is_owned_before_deleting(self):
        (self.state / "create-pending").touch()
        with patch.object(pods, "api", side_effect=[None, self.record, self.record, {}, None, None]) as api, patch.object(pods, "backoff"):
            pods.delete(self.state)
        self.assertEqual([c.args[0] for c in api.call_args_list], ["GET", "GET", "GET", "DELETE", "GET", "GET"])

    def test_delete_keeps_key_on_auth_failure(self):
        with patch.object(pods, "api", side_effect=RuntimeError("HTTP 403")):
            with self.assertRaisesRegex(RuntimeError, "HTTP 403"):
                pods.delete(self.state)
        self.assertTrue((self.state / "identity").is_file())
        self.assertFalse((self.state / "deleted").exists())


class PodStartupTests(unittest.TestCase):
    template = {"pod_id": "eur0123", "description": "expected owner"}
    deadline = float("inf")

    def test_replaces_one_unrouted_pod_only_after_confirmed_deletion(self):
        for deleted, calls in ((True, 2), (False, 1)):
            with self.subTest(deleted=deleted), TemporaryDirectory() as tmp:
                state = Path(tmp) / "tapis"
                def unrouted(state, *_args):
                    state.mkdir()
                    if deleted:
                        (state / "deleted").touch()
                    raise pods.MissingIngressRoute("default certificate")
                with patch.object(pods, "create_once", side_effect=unrouted) as provision:
                    with self.assertRaises(pods.MissingIngressRoute):
                        pods.create(state, Path(tmp), "image", 8192, 1000)
                self.assertEqual(provision.call_count, calls)
                self.assertEqual((Path(tmp) / "tapis-unrouted/deleted").exists(), deleted)
                if deleted:
                    self.assertEqual(provision.call_args_list[0], provision.call_args_list[1])

    def test_identifies_only_an_unrouted_tls_handshake(self):
        errors = b"depth=0 CN = TRAEFIK DEFAULT CERT\nverify error:num=18:self-signed certificate"
        for rc, stderr, missing in ((255, errors, True), (0, errors, False),
                                    (255, b"Permission denied", False), (255, b"self-signed certificate", False)):
            self.assertEqual(pods.missing_ingress(subprocess.CompletedProcess([], rc, stderr=stderr)), missing)
        self.assertFalse(pods.missing_ingress(None))

    def test_ambiguous_create_recovers_existing_pod(self):
        for error in (pods.TransientError("502"), pods.ConflictError("409")):
            with self.subTest(error=error), patch.object(pods, "api", side_effect=[error, None, self.template]) as api, patch.object(pods, "backoff"):
                pods.submit(self.template, self.deadline)
                self.assertEqual([c.args[0] for c in api.call_args_list], ["POST", "GET", "GET"])

    def test_ambiguous_create_retries_only_original_payload(self):
        with patch.object(pods, "api", side_effect=[pods.TransientError(), None, None, None, {}]) as api, patch.object(pods, "backoff"):
            pods.submit(self.template, self.deadline)
        self.assertEqual(api.call_args_list[0], api.call_args_list[-1])
        self.assertEqual([c.args[0] for c in api.call_args_list], ["POST", "GET", "GET", "GET", "POST"])

    def test_foreign_conflict_stops_without_replaying_or_deleting(self):
        with patch.object(pods, "api", side_effect=[pods.ConflictError(), {"description": "foreign"}]) as api, patch.object(pods, "backoff"):
            with self.assertRaisesRegex(RuntimeError, "another attempt"):
                pods.submit(self.template, self.deadline)
        self.assertEqual([c.args[0] for c in api.call_args_list], ["POST", "GET"])

    def test_only_reads_and_host_attestation_get_transport_retries(self):
        for op, path, payload in (("GET", "/id", None), ("POST", "/id/exec", pods.HOST_KEY)):
            with self.subTest(op=op), patch.object(pods, "request", side_effect=[pods.TransientError(), "ok"]), patch.object(pods, "backoff"):
                self.assertEqual(pods.api(op, path, payload, deadline=self.deadline), "ok")
        with patch.object(pods, "request", side_effect=pods.TransientError()) as send:
            with self.assertRaises(pods.TransientError):
                pods.api("POST", "/id/exec", {"commands": [["other"]]}, deadline=self.deadline)
            send.assert_called_once()

    def test_startup_slot_released_after_exception(self):
        with TemporaryDirectory() as directory, patch.dict(pods.os.environ, {"XDG_CACHE_HOME": directory, "TAPIS_MAX_STARTING": "1"}):
            with self.assertRaises(ValueError):
                with pods.startup_slot():
                    raise ValueError()
            with pods.startup_slot():
                self.assertTrue((Path(directory) / "eureka/tapis-starts/0").is_file())

    def test_startups_wait_for_capacity_and_release_slots_when_ready(self):
        waiting = Event()
        resumed = Event()

        def pause(_seconds):
            waiting.set()
            resumed.wait(5)

        def start():
            with pods.startup_slot():
                entered.set()

        entered = Event()
        with TemporaryDirectory() as directory:
            env = {"XDG_CACHE_HOME": directory, "TAPIS_MAX_STARTING": "1"}
            with patch.dict(pods.os.environ, env), patch.object(pods.time, "sleep", side_effect=pause):
                with pods.startup_slot():
                    worker = Thread(target=start, daemon=True)
                    worker.start()
                    self.assertTrue(waiting.wait(5))
                    self.assertFalse(entered.is_set())
                resumed.set()
                worker.join(5)
                self.assertTrue(entered.is_set())
