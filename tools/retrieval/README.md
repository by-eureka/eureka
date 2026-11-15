# Retrieval tools

An optional MCP server that gives Eureka's agents Lean search tools.
It needs the native Lean/Beam image from [runtime setup](../../harness/deploy/README.md). `scripts/setup-retrieval.sh` creates the internal `harness-agents` network.

From the repository root, build the agent image and start the shared service:

```sh
docker build --target client -t eureka-agent:local tools/retrieval
docker compose -f tools/retrieval/compose.yaml up -d --build --wait
```

Then add these to the usual harness command:

```sh
--image eureka-agent:local --network harness-agents --mcp tools/retrieval/mcp.json
```

## Tools

- `mcp_retrieval_lean_local_search`: a heuristic text search over the attempt's own source and packages. It can't find generated declarations; use Beam's `lean_workspace_symbols` for those.
- `mcp_retrieval_lean_loogle`: Loogle, run by the service against a prebuilt index of the image's Mathlib. It refuses requests from a different Lean/Mathlib snapshot and never falls back to public Loogle.
- `mcp_retrieval_lean_leansearch` and `mcp_retrieval_lean_leanfinder`: the public LeanSearch and Lean Finder services.
- `mcp_retrieval_lean_hammer_premise`: a public premise-selection service. Pass one goal's text from Beam's `lean_goals` as `query`; it returns suggested lemma names.

The three hosted tools send the query or goal text to their providers.
Their indexes may not match the project's Mathlib, so check any lemma they suggest with Lean.
Outages and rate limits come back as tool errors rather than empty results, and the rate limits are shared by all attempts.

## Security

Only the service has internet access, and its API runs a fixed set of searches; it never fetches arbitrary URLs or runs commands.
It doesn't mount attempts, proof archives or the harness store, and it publishes no host port.
Keep it on a trusted internal Docker network.

## Resources and rebuilding

The service runs one Loogle process, so give it about 12 GB of RAM.
The first image build compiles Loogle and indexes Mathlib; later starts reuse that index.
If you change Beam or Mathlib, rebuild both images and extract matching packages again as described in the [runtime setup](../../harness/deploy/README.md).
To use a different Beam image, pass `--build-arg BEAM_IMAGE=...` when building the client and set `BEAM_IMAGE=...` for Compose.

To stop just the retrieval service, run `docker compose -f tools/retrieval/compose.yaml down`.

## Tests

The offline tests need Python 3.12 or later:

```sh
python3 -m venv tools/retrieval/.venv
tools/retrieval/.venv/bin/pip install -r tools/retrieval/requirements.txt
tools/retrieval/.venv/bin/python -m unittest discover -s tools/retrieval/tests -v
```

There is also an end-to-end test. With the runtime image built and retrieval running, set `HARNESS_PACKAGES` and `HARNESS_IMAGE` and run:

```sh
HARNESS_RETRIEVAL_IMAGE=eureka-agent:local python3 harness/tests/lifecycle.py Lifecycle.test_retrieval_in_sandbox -v
```

It uses scripted model responses and only the local searches, so it costs nothing.

Upstream code and provider attribution are in [NOTICE](NOTICE).
