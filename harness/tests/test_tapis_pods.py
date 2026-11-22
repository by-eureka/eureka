import importlib.util
import json
import subprocess
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
from tempfile import TemporaryDirectory
from threading import Event, Thread
import unittest
from unittest.mock import ANY, call, patch


source = Path(__file__).resolve().parents[2] / "scripts/tapis-pod.py"
spec = importlib.util.spec_from_file_location("tapis_pod", source)
pods = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pods)


class StartupProbeTests(unittest.TestCase):
    def test_manifest_read_recovers_after_disconnect(self):
        command = ["ssh", "pod", "encoded-read"]
        replies = [subprocess.CompletedProcess(command, 255),
                   subprocess.CompletedProcess(command, 0, stdout=b'{"packages": []}')]
        with patch.object(pods, "ssh", return_value=command) as ssh, \
             patch.object(pods.subprocess, "run", side_effect=replies) as run, patch.object(pods, "backoff"):
            self.assertEqual(pods.read_startup_probe("state", ["cat", "/project/lake-manifest.json"], 20),
                             b'{"packages": []}')
        ssh.assert_called_once_with("state", ["exec", "cat", "/project/lake-manifest.json"])
        self.assertEqual([c.args[0] for c in run.call_args_list], [command, command])

    def test_remote_command_errors_and_repeated_disconnects(self):
        for status, calls in [(1, 1), (255, 3)]:
            with self.subTest(status=status), patch.object(pods, "ssh", return_value=["ssh"]), \
                 patch.object(pods.subprocess, "run", return_value=subprocess.CompletedProcess(["ssh"], status)) as run, \
                 patch.object(pods, "backoff"):
                with self.assertRaises(subprocess.CalledProcessError) as error:
                    pods.read_startup_probe("state", ["uname", "-m"], 25)
                self.assertEqual(error.exception.returncode, status)
                self.assertEqual(run.call_count, calls)


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
                def unrouted(state, *_args, **_kwargs):
                    state.mkdir()
                    if deleted:
                        (state / "deleted").touch()
                    raise pods.IngressCertificateUnavailable("first", "replacement")
                with patch.object(pods, "claim_certified_name", side_effect=["first", "replacement"]), patch.object(pods, "create_once", side_effect=unrouted) as provision:
                    with self.assertRaises(pods.IngressCertificateUnavailable):
                        pods.create(state, Path(tmp), "image", 8192, 1000)
                self.assertEqual(provision.call_count, calls)
                self.assertEqual((Path(tmp) / "tapis-unrouted/deleted").exists(), deleted)
                if deleted:
                    first, second = (c.args for c in provision.call_args_list)
                    self.assertEqual(first[:-1], second[:-1])
                    self.assertEqual((first[-1], second[-1]), ("first", "replacement"))
                    self.assertEqual([c.kwargs["allow_replacement"] for c in provision.call_args_list], [True, False])

    def test_only_confirmed_deleted_ready_names_can_be_reused_once(self):
        ident = "eur0123456789abcdef"
        with TemporaryDirectory() as tmp, patch.dict(pods.os.environ, {"XDG_CACHE_HOME": tmp}):
            state = Path(tmp) / "state"
            state.mkdir()
            (state / "pod.json").write_text(json.dumps({"id": ident, "description": "owned"}))
            (state / "runtime.json").touch()
            with patch.object(pods, "api", side_effect=[{"description": "owned"}, {}, None]):
                pods.delete(state)
            self.assertEqual(pods.claim_certified_name(), ident)
            pods.delete(state)
            self.assertIsNone(pods.claim_certified_name())

    def test_concurrent_attempts_claim_each_name_once(self):
        with TemporaryDirectory() as tmp, patch.dict(pods.os.environ, {"XDG_CACHE_HOME": tmp}):
            names = {f"eur{n:016x}" for n in range(3)}
            for name in names:
                (pods.certified_names() / name).touch()
            with ThreadPoolExecutor(max_workers=8) as pool:
                claimed = list(pool.map(lambda _: pods.claim_certified_name(), range(8)))
            self.assertCountEqual(claimed, [*names, None, None, None, None, None])

    def test_default_certificate_can_recover_in_place_with_no_cached_name(self):
        # The provider may finish issuance after 90 seconds. Keep the original pod in that case.
        with TemporaryDirectory() as tmp:
            state = Path(tmp) / "tapis"
            available = {"status": "AVAILABLE"}
            result = [{"stdout": "ssh-ed25519 YQ=="}]
            def api(verb, path, *args, **kwargs):
                if verb == "POST":
                    return result
                available["description"] = json.loads((state / "pod.json").read_text())["description"]
                return available
            default = subprocess.CompletedProcess([], 255, stderr=b"TRAEFIK DEFAULT CERT self-signed certificate")
            ok = subprocess.CompletedProcess([], 0, stdout=b"recovered")
            def run(cmd, **kwargs):
                if cmd[0] == "ssh-keygen":
                    (state / "identity.pub").write_text("ssh-ed25519 YQ==")
                    return
                return next(probes)
            probes = iter((default, default, ok))
            now = [0]
            def advance(*args):
                now[0] += 100
            (Path(tmp) / "lean-toolchain").write_text("test")
            with patch.object(pods, "api", side_effect=api), patch.object(pods, "submit"), \
                 patch.object(pods.time, "monotonic", side_effect=lambda: now[0]), \
                 patch.object(pods, "backoff", side_effect=advance), patch.object(pods, "delete"), \
                 patch.object(pods, "claim_certified_name", return_value=None) as claim, \
                 patch.object(pods.subprocess, "run", side_effect=run), \
                 patch.object(pods.subprocess, "check_output", return_value=b"0" * 40):
                with self.assertRaisesRegex(RuntimeError, "compiler revisions differ"):
                    pods.create_once(state, Path(tmp), "debganguly/infinilean:test", 8192, 1000, 900,
                                     "eur0123456789abcdef", allow_replacement=True)
                claim.assert_called_once_with()

    def test_identifies_only_an_unrouted_tls_handshake(self):
        errors = b"depth=0 CN = TRAEFIK DEFAULT CERT\nverify error:num=18:self-signed certificate"
        for rc, stderr, missing in ((255, errors, True), (0, errors, False),
                                    (255, b"Permission denied", False), (255, b"self-signed certificate", False)):
            self.assertEqual(pods.default_ingress_certificate(subprocess.CompletedProcess([], rc, stderr=stderr)), missing)
        self.assertFalse(pods.default_ingress_certificate(None))

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
