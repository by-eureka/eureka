import importlib.util
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import call, patch


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
        self.assertEqual(api.call_args_list, [call("GET", "/eur0123"),
                                              call("DELETE", "/eur0123"),
                                              call("GET", "/eur0123")])
        self.assertTrue((self.state / "deleted").is_file())
        self.assertFalse((self.state / "identity").exists())

    def test_never_deletes_a_different_operators_pod(self):
        with patch.object(pods, "api", return_value={"description": "another attempt"}) as api:
            with self.assertRaisesRegex(RuntimeError, "another attempt"):
                pods.delete(self.state)
        api.assert_called_once_with("GET", "/eur0123")
        self.assertFalse((self.state / "deleted").exists())
        self.assertTrue((self.state / "identity").is_file())
