"""Offline boundary tests; no public search or model requests."""

import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

import httpx
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from backends import Backends, Loogle, SearchError, SearchRequest
from local import local_search
from service import create_app

SNAPSHOT = {"lean_toolchain": "leanprover/lean4:v4.35.0", "mathlib_revision": "fixture"}


class SearchTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.calls = []
        self.response = httpx.Response(200, json=[])

        def respond(request):
            self.calls.append(request)
            return self.response

        self.http = httpx.AsyncClient(transport=httpx.MockTransport(respond), follow_redirects=False)
        self.loogle = AsyncMock()
        self.loogle.query.return_value = [{"name": "Nat.add_comm"}]
        self.backends = Backends(self.loogle, SNAPSHOT, self.http)
        self.client = httpx.AsyncClient(
            transport=httpx.ASGITransport(app=create_app(self.backends)), base_url="http://retrieval"
        )

    async def asyncTearDown(self):
        await self.client.aclose()
        await self.http.aclose()

    async def test_provider_payloads_and_result_limits(self):
        fixtures = [
            (
                "leansearch",
                [[{"result": {"name": ["Nat", "add_comm"], "module_name": ["Init"]}}] * 3],
                {"query": ["addition"], "num_results": 1},
                "name",
            ),
            (
                "leanfinder",
                {"results": [{"formal_name": "Nat.add_comm"}] * 3},
                {"inputs": "addition", "top_k": 1, "version": "v4.24.0"},
                "formal_name",
            ),
            (
                "hammer",
                [{"name": "Nat.add_comm"}] * 3,
                {"state": "addition", "new_premises": [], "k": 1},
                "name",
            ),
        ]
        for provider, data, payload, key in fixtures:
            with self.subTest(provider=provider):
                self.response = httpx.Response(200, json=data)
                result = await self.backends.search(
                    SearchRequest(provider=provider, query="addition", num_results=1, version="v4.24.0")
                )
                self.assertEqual(len(result["items"]), 1)
                self.assertEqual(result["items"][0][key], "Nat.add_comm")
                self.assertEqual(json.loads(self.calls[-1].content), payload)
                self.assertEqual(str(self.calls[-1].url), Backends.URLS[provider])

    async def test_loogle_requires_matching_snapshot_and_never_uses_http(self):
        for snapshot in (None, {}, dict(SNAPSHOT, mathlib_revision="different")):
            with self.assertRaisesRegex(SearchError, "snapshot"):
                await self.backends.search(
                    SearchRequest(provider="loogle", query="Nat.add_comm", snapshot=snapshot)
                )
        result = await self.backends.search(
            SearchRequest(provider="loogle", query="Nat.add_comm", snapshot=SNAPSHOT)
        )
        self.assertEqual(result["source"], "local-loogle")
        self.loogle.query.assert_awaited_once()
        self.assertEqual(self.calls, [])

    async def test_service_rejects_unknown_operations_urls_and_oversized_requests(self):
        for extra in (
            {"provider": "shell"},
            {"url": "http://host/"},
            {"num_results": 1000},
            {"num_results": True},
            {"query": " "},
            {"query": "x" * 8193},
            {"version": "latest"},
        ):
            result = await self.client.post("/search", json={"provider": "hammer", "query": "x", **extra})
            self.assertEqual(result.status_code, 400)
        result = await self.client.post("/search", content=b"x" * 65537)
        self.assertEqual(result.status_code, 413)
        self.assertEqual(self.calls, [])

    async def test_shared_rate_limit(self):
        for _ in range(6):
            result = await self.client.post("/search", json={"provider": "hammer", "query": "goal"})
            self.assertEqual(result.status_code, 200)
        result = await self.client.post("/search", json={"provider": "hammer", "query": "goal"})
        self.assertIn("shared limit", result.json()["error"])
        self.assertEqual(len(self.calls), 6)

    async def test_malformed_leansearch_response_is_not_an_empty_result(self):
        self.response = httpx.Response(200, json={})
        result = await self.client.post("/search", json={"provider": "leansearch", "query": "goal"})
        self.assertIn("malformed", result.json()["error"])

    async def test_upstream_failures_and_redirects_are_errors_not_empty_hits(self):
        for response in (
            httpx.Response(500),
            httpx.Response(302, headers={"location": "http://host/"}),
            httpx.Response(200, content=b"not json"),
            httpx.Response(200, json={"wrong": []}),
            httpx.Response(200, content=b"x" * (2 * 1024 * 1024 + 1)),
        ):
            self.response = response
            with patch("service.logger.exception"):
                result = await self.client.post("/search", json={"provider": "leanfinder", "query": "goal"})
            self.assertGreaterEqual(result.status_code, 400)
            self.assertIn("error", result.json())
            self.assertNotIn("items", result.json())
        self.assertEqual(len(self.calls), 5, "followed an upstream redirect")


class LocalTests(unittest.IsolatedAsyncioTestCase):
    @unittest.skipUnless(Path("/proc").exists(), "Linux process-state check")
    async def test_close_kills_lake_wrapper_and_its_child(self):
        loogle = Loogle(Path.cwd(), "unused", "unused")
        child = "import time; time.sleep(60)"
        wrapper = (
            "import subprocess, sys, json; "
            "p=subprocess.Popen([sys.executable, '-c', sys.argv[1]]); "
            "print('Loogle is ready.', flush=True); "
            "sys.stdin.readline(); "
            "print(json.dumps({'hits':[{'name':str(p.pid)}]}), flush=True); p.wait()"
        )
        loogle.command = [sys.executable, "-u", "-c", wrapper, child]
        try:
            pid = int((await loogle.query("child", 1))[0]["name"])
            group = loogle.process.pid
            self.assertEqual(os.getpgid(pid), group)
        finally:
            await loogle.close()
        # The host's init may not have reaped the orphan yet; a zombie is stopped.
        state = Path(f"/proc/{pid}/stat")
        if state.exists():
            self.assertIn(state.read_text().split()[2], ("Z", "X"))

    async def test_names_packages_and_no_symlink_escape(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            root = Path(directory)
            (root / "Proof.lean").write_text(
                "namespace Demo\nsection\n@[simp] theorem foo : True := by trivial\nend\nend Demo\n"
            )
            package = root / ".lake/packages/demo"
            package.mkdir(parents=True)
            (package / "Defs.lean").write_text("namespace Other\ndef foo := 1\nend Other\n")
            secret = Path(outside) / "Hidden.lean"
            secret.write_text("theorem foo_secret : True := by trivial\n")
            (root / "Escape.lean").symlink_to(secret)
            result = await local_search(root, "Demo.foo", 10)
            self.assertEqual([x["name"] for x in result["items"]], ["Demo.foo"])
            result = await local_search(root, "foo", 10)
            self.assertEqual({x["name"] for x in result["items"]}, {"Demo.foo", "Other.foo"})
            self.assertEqual(result["index"], "unavailable")
            self.assertFalse((await local_search(root, "foo.*", 10))["items"])

    async def test_stdio_discovery_validation_and_local_search_without_service(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Example.lean").write_text("theorem fixture_name : True := by trivial\n")
            params = StdioServerParameters(
                command=sys.executable,
                args=[str(ROOT / "server.py"), "--endpoint", "http://127.0.0.1:1", "--project", directory],
            )
            async with stdio_client(params) as (read, write), ClientSession(read, write) as session:
                await session.initialize()
                listed = await session.list_tools()
                self.assertEqual(
                    {t.name for t in listed.tools},
                    {
                        "lean_local_search",
                        "lean_loogle",
                        "lean_leansearch",
                        "lean_leanfinder",
                        "lean_hammer_premise",
                    },
                )
                result = await session.call_tool("lean_local_search", {"query": "fixture_name"})
                self.assertFalse(result.isError)
                self.assertEqual(result.structuredContent["items"][0]["name"], "fixture_name")
                invalid = await session.call_tool("lean_leansearch", {"query": "x", "num_results": 0})
                self.assertTrue(invalid.isError)
                offline = await session.call_tool("lean_leansearch", {"query": "x"})
                self.assertTrue(offline.isError)

    async def test_loogle_serialization_bad_queries_and_restart_after_desync(self):
        script = """import sys, json, time
print("Loogle is ready.", flush=True)
for line in sys.stdin:
    q = line.strip()
    if q == 'crash': sys.exit(1)
    if q == 'slow': time.sleep(30)
    if q == 'garbage': print('invalid json', flush=True)
    elif q == 'invalid': print(json.dumps({'error': 'bad syntax'}), flush=True)
    else: print(json.dumps({'hits': [{'name': q, 'type': 'test', 'module': 'Test'}]}), flush=True)
"""
        loogle = Loogle(Path.cwd(), "unused", "unused")
        loogle.command = [sys.executable, "-u", "-c", script]
        try:
            results = await asyncio.gather(*(loogle.query(str(n), 1) for n in range(8)))
            self.assertEqual([r[0]["name"] for r in results], list(map(str, range(8))))
            self.assertEqual((await loogle.query("one\ntwo", 1))[0]["name"], "one two")
            process = loogle.process
            with self.assertRaisesRegex(SearchError, "bad syntax"):
                await loogle.query("invalid", 1)
            self.assertIs(loogle.process, process)
            for query in ("crash", "garbage"):
                with self.assertRaises(ValueError):
                    await loogle.query(query, 1)
                self.assertIsNone(loogle.process)
                self.assertEqual((await loogle.query("recovered", 1))[0]["name"], "recovered")
            process = loogle.process
            with self.assertRaises(TimeoutError):
                async with asyncio.timeout(0.05):
                    await loogle.query("slow", 1)
            self.assertIsNone(loogle.process)
            self.assertIsNotNone(process.returncode, "cancelled query left a subprocess alive")
            self.assertEqual((await loogle.query("after_timeout", 1))[0]["name"], "after_timeout")
        finally:
            await loogle.close()


if __name__ == "__main__":
    unittest.main()
