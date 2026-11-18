import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[2] / "scripts/prepare-verisoftbench.py"
spec = importlib.util.spec_from_file_location("verisoft", SCRIPT)
verisoft = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verisoft)


class VeriSoftBench(unittest.TestCase):
    def repo(self, root, files):
        root.mkdir()
        for name, text in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(["git", "init", "-q", root], check=True)
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        subprocess.run(["git", "-C", root, "-c", "user.name=test", "-c", "user.email=test@localhost",
                        "commit", "-qm", "fixture"], check=True)
        return verisoft.git(root, "rev-parse", "HEAD").decode().strip()

    def test_prepares_original_paths_from_pinned_blobs(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            repo, bench = root / "src", root / "bench"
            rows = [dict(id=i, lean_root="Sample", rel_path=f"Lib/{i}.lean", thm_name=name,
                         thm_stmt=f"theorem {name} : True", ground_truth_proof=":= by trivial")
                    for i, name in ((1, "one"), (2, "two"))]
            commit = self.repo(repo, {
                "lean-toolchain": "leanprover/lean4:v4.24.0\n",
                "lakefile.toml": 'name = "sample"\n[[lean_lib]]\nname = "Lib"\n',
                **{r["rel_path"]: f'{r["thm_stmt"]} {r["ground_truth_proof"]}\n' for r in rows},
                "notes.txt": "solution notes",
            })
            release = self.repo(bench, {
                "repos.json": json.dumps({"repos": {"Sample": {"commit": commit}}}),
                "data/verisoftbench.jsonl": "\n".join(map(json.dumps, rows)),
            })
            (repo / rows[0]["rel_path"]).write_text("uncommitted local edits")
            out = root / "task"
            with patch.object(verisoft, "BENCHMARK", release):
                verisoft.prepare(bench, repo, ["1", "2"], out)
            self.assertEqual(json.loads((out / "proof-files.json").read_text()), [r["rel_path"] for r in rows])
            self.assertEqual(json.loads((out / "config.json").read_text())["theorem_names"], ["one", "two"])
            challenge = (out / "Proofs/Challenge.lean").read_text()
            self.assertIn("leanprover/lean4:v4.24.0", challenge)
            self.assertIn((out / "lean-toolchain").read_text().strip(), challenge)
            for r in rows:
                self.assertEqual((out / r["rel_path"]).read_text(), r["thm_stmt"] + " := by sorry\n")
            self.assertFalse((out / ".git").exists())
            self.assertFalse((out / "notes.txt").exists())

    def test_ambiguous_or_missing_proof_fails(self):
        task = dict(id=1, thm_stmt="theorem t : True", ground_truth_proof=":= by trivial")
        source = "theorem t : True := by trivial\n"
        for text in ("", source + source):
            with self.assertRaisesRegex(ValueError, "one exact"):
                verisoft.remove_proof(text, task)
