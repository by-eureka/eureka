"""Exercise stop PID checks without a model, Docker, or a real proof run."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class RunScripts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.run = Path(self.temp.name) / 'old'
        self.run.mkdir()
        (self.run / 'run.log').touch()

    def process(self, name='harness', store=None):
        process = subprocess.Popen(
            ['bash', '-c', 'exec -a "$1" "$2" -c "$3" --store "$4"', 'fixture',
             name, sys.executable, 'import time; print("ready", flush=True); time.sleep(60)',
             str(store or self.run / 'store')], stdout=subprocess.PIPE, text=True,
        )
        def cleanup():
            if process.poll() is None:
                process.kill()
            process.wait()
            process.stdout.close()
        self.addCleanup(cleanup)
        self.assertEqual(process.stdout.readline(), 'ready\n')
        (self.run / 'pid').write_text(str(process.pid))
        return process

    def stop(self):
        return subprocess.run([str(ROOT / 'scripts/stop.sh'), str(self.run)],
                              capture_output=True, text=True, timeout=5)

    def test_stop_signals_only_the_matching_run(self):
        process = self.process()
        result = self.stop()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(process.wait(), -15)

    def test_stop_refuses_unrelated_process_and_other_run(self):
        for name, store in [('python3', self.run / 'store'),
                            ('harness', self.run / 'other-store')]:
            with self.subTest(name=name, store=store):
                process = self.process(name, store)
                result = self.stop()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('refusing to stop', result.stderr)
                self.assertIsNone(process.poll())

    def test_stop_does_not_wait_for_a_zombie(self):
        process = self.process()
        process.kill()
        os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOWAIT)
        result = self.stop()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_stop_rejects_process_group_ids(self):
        for pid in ('0', '-1', '1'):
            with self.subTest(pid=pid):
                (self.run / 'pid').write_text(pid)
                result = self.stop()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('invalid PID', result.stderr)

    def test_restart_keeps_latest_compaction_and_saved_proof(self):
        # Exercise the real restart script. Only launching the harness is stubbed.
        root = Path(self.temp.name)
        scripts = root / 'scripts'
        scripts.mkdir()
        shutil.copy2(ROOT / 'scripts/restart.sh', scripts / 'restart.sh')
        (scripts / 'env.sh').write_text(f"ROOT={str(root)!r}\nneed_harness() {{ :; }}\n")
        launch = scripts / 'run.sh'
        launch.write_text('#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\n'
                          'p=Path(sys.argv[2]);p.mkdir()\n'
                          '(p/"launch.json").write_text(json.dumps(sys.argv[1:]))\n')
        launch.chmod(0o755)
        pool = self.run / 'store/pool-fixture'
        (pool / 'source').mkdir(parents=True)
        attempt = pool / 'attempts/attempt-000001'
        attempt.mkdir(parents=True)
        (attempt / 'candidate.lean').write_text('saved proof state')
        (attempt / 'result.json').write_text(json.dumps({'status': 'not_submitted',
            'last_message': 'recent plan', 'detail': 'verifier feedback'}))
        for number, text in [(20, 'older summary'), (10000, 'newest summary')]:
            directory = attempt / f'compaction-{number:04}'
            directory.mkdir()
            (directory / 'handoff.txt').write_text(text)
        new = root / 'new'
        result = subprocess.run([str(scripts / 'restart.sh'), str(self.run), str(new)],
                                 capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads((new / 'restart.json').read_text())
        self.assertIn('newest summary', metadata['prompt'])
        self.assertNotIn('older summary', metadata['prompt'])
        self.assertIn('old IDs are no longer active', metadata['prompt'])
        self.assertEqual(metadata['seed_sha256'], hashlib.sha256(b'saved proof state').hexdigest())

        paths = ['A.lean', 'Lib/B.lean']
        (pool / 'source' / 'proof-files.json').write_text(json.dumps(paths))
        bundle = attempt / 'candidate'
        (bundle / 'Lib').mkdir(parents=True)
        for name in paths:
            (bundle / name).write_text(name)
        new = root / 'multi'
        result = subprocess.run([str(scripts / 'restart.sh'), str(self.run), str(new)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads((new / 'restart.json').read_text())
        self.assertEqual(metadata['seed'], str(bundle))
        self.assertEqual(metadata['seed_files_sha256'], {name: hashlib.sha256(name.encode()).hexdigest()
                                                         for name in paths})


if __name__ == '__main__':
    unittest.main()
