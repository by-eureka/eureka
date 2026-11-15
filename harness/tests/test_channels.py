"""Exercise development launch and service isolation without Docker or a model."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
SETTINGS = ('BEAM_IMAGE', 'AGENT_IMAGE', 'RETRIEVAL_IMAGE', 'RETRIEVAL_PROJECT',
            'RETRIEVAL_NETWORK', 'PACKAGES', 'IMAGE', 'NETWORK', 'RETRIEVAL',
            'API_KEY_FILE', 'ELAN_HOME')


class Channels(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        scripts = self.root / 'scripts'
        scripts.mkdir()
        for name in ('env.sh', 'run.sh', 'setup-retrieval.sh', 'stop-services.sh'):
            shutil.copy2(ROOT / 'scripts' / name, scripts / name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'docker.jsonl'
        self.env = {key: value for key, value in os.environ.items() if key not in SETTINGS}
        self.env.update(PATH=str(self.bin) + ':' + os.environ['PATH'],
                        DOCKER_LOG=str(self.log), DEEPSEEK_API_KEY='local-test-only')
        self.executable(self.bin / 'docker', '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with Path(os.environ['DOCKER_LOG']).open('a') as log:
    log.write(json.dumps({'args': args, 'image': os.environ.get('RETRIEVAL_IMAGE'),
                         'network': os.environ.get('RETRIEVAL_NETWORK')}) + '\\n')
if args[:2] == ['network', 'inspect']:
    if '--format' in args:
        print(os.environ.get('TEST_NETWORK_INTERNAL', 'true'))
    elif os.environ.get('TEST_NETWORK_EXISTS') != '1':
        sys.exit(1)
''')
        for name in ('lake', 'bwrap', 'systemd-run'):
            self.executable(self.bin / name, '#!/bin/sh\nexit 0\n')
        harness = self.root / 'harness/target/debug/harness'
        harness.parent.mkdir(parents=True)
        self.executable(harness, '''#!/usr/bin/env python3
import json, sys
from pathlib import Path
store = Path(sys.argv[sys.argv.index('--store') + 1])
(store / 'argv.json').write_text(json.dumps(sys.argv[1:]))
''')

    @staticmethod
    def executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def script(self, name, *args, **settings):
        return subprocess.run([str(self.root / 'scripts' / name), *map(str, args)],
                              env=dict(self.env, **settings), capture_output=True,
                              text=True, timeout=10)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_default_launches_use_dev_images_and_packages(self):
        for retrieval, image, network in [('0', 'eureka-lean:dev', 'none'),
                                          ('1', 'eureka-agent:dev', 'eureka-dev-agents')]:
            with self.subTest(retrieval=retrieval):
                run = self.root / ('run-' + retrieval)
                result = self.script('run.sh', self.root, run, RETRIEVAL=retrieval)
                self.assertEqual(result.returncode, 0, result.stderr)
                saved = run / 'store/argv.json'
                for _ in range(100):
                    if saved.exists():
                        break
                    time.sleep(0.02)
                argv = json.loads(saved.read_text())
                self.assertEqual(argv[argv.index('--image') + 1], image)
                self.assertEqual(argv[argv.index('--network') + 1], network)
                self.assertEqual(argv[argv.index('--packages') + 1], str(self.root / 'lean-packages-dev'))
                self.assertEqual('--mcp' in argv, retrieval == '1')

    def test_retrieval_setup_and_stop_use_the_dev_project(self):
        for script in ('setup-retrieval.sh', 'stop-services.sh'):
            result = self.script(script)
            self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn(['network', 'create', '--internal', 'eureka-dev-agents'],
                      [c['args'] for c in calls])
        build, = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertEqual(build[build.index('-t') + 1], 'eureka-agent:dev')
        self.assertIn('BEAM_IMAGE=eureka-lean:dev', build)
        compose = [c for c in calls if c['args'][0] == 'compose']
        self.assertEqual(len(compose), 2)
        for call in compose:
            self.assertEqual(call['args'][1:3], ['-p', 'eureka-retrieval-dev'])
            self.assertEqual(call['image'], 'eureka-retrieval:dev')
            self.assertEqual(call['network'], 'eureka-dev-agents')
        self.assertEqual(compose[-1]['args'][-1], 'down')

    def test_custom_runtime_settings_reach_both_setup_and_cleanup(self):
        settings = dict(BEAM_IMAGE='example/lean:test', AGENT_IMAGE='example/agent:test',
                        RETRIEVAL_IMAGE='example/retrieval:test', RETRIEVAL_PROJECT='my-dev',
                        RETRIEVAL_NETWORK='my-dev-agents', TEST_NETWORK_EXISTS='1')
        for script in ('setup-retrieval.sh', 'stop-services.sh'):
            result = self.script(script, **settings)
            self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertFalse(any(c['args'][:2] == ['network', 'create'] for c in calls))
        build, = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertIn('BEAM_IMAGE=example/lean:test', build)
        self.assertEqual(build[build.index('-t') + 1], 'example/agent:test')
        for call in [c for c in calls if c['args'][0] == 'compose']:
            self.assertEqual(call['args'][1:3], ['-p', 'my-dev'])
            self.assertEqual(call['image'], 'example/retrieval:test')
            self.assertEqual(call['network'], 'my-dev-agents')

    def test_existing_public_network_is_rejected_before_build_or_start(self):
        result = self.script('setup-retrieval.sh', TEST_NETWORK_EXISTS='1',
                             TEST_NETWORK_INTERNAL='false')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('must be an internal Docker network', result.stderr)
        self.assertTrue(all(c['args'][:2] == ['network', 'inspect'] for c in self.calls()))


if __name__ == '__main__':
    unittest.main()
