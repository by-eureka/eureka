# Beam pool for the harness

Every attempt runs in its own Docker sandbox, built from Beam's pool image.
The sandboxes share a Beam pool (a gateway plus workers) and reach it over an internal Docker network with no internet access.

## Setup

Build the image (Lean 4.33, a prepared Mathlib, and Beam) from the lean-beam checkout:

    docker build -f lean-beam/deploy/beam-pool/Dockerfile -t beam-pool:local lean-beam

Copy the image's prepared packages out once. Every sandbox mounts them read-only:

    docker create --name pool-extract beam-pool:local
    docker cp pool-extract:/project/.lake/packages /path/to/pool-packages
    docker rm pool-extract

Start the pool. `pool-override.yaml` puts the gateway on `harness-agents`, the internal network the sandboxes use.
The workers stay on Compose's own internal network.

    (umask 077; python3 -c 'import secrets; print(secrets.token_hex(32))' > /path/to/pool.token)
    BEAM_POOL_TOKEN=$(cat /path/to/pool.token) docker compose \
        -f lean-beam/deploy/beam-pool/compose.yaml -f harness/deploy/pool-override.yaml \
        up -d --scale worker=4

Then pass `--packages /path/to/pool-packages --pool-token-file /path/to/pool.token` to the harness.

Problem templates have to match the image's layout: its `lakefile.toml`, `lake-manifest.json` and `lean-toolchain`, an empty root `Proofs.lean`, `Proofs/Challenge.lean`, `Proofs/Solution.lean`, and a `config.json` naming the modules `Proofs.Challenge` and `Proofs.Solution`.
`examples/simple` is a working example.

## Using the pool on Tapis

Instead of a local pool, the sandboxes can use a Beam pool deployed on Tapis Pods (see `lean-beam/deploy/beam-pool/TAPIS.md` for deploying one).
Tapis only exposes the gateway as TLS with SNI on port 443, and the sandboxes have no internet access.
So `tapis-bridge.yaml` runs a small stunnel container that sits on `harness-agents` and an outbound network.
The sandboxes reach it as `tapis-gateway:9000`, and it forwards to the Tapis gateway after checking its certificate.

You need the pool's `BEAM_POOL_TOKEN` (the one given to `tapis.py up`, not the local pool's) in `.secrets/tapis-pool.token`, and the gateway host.
`python3 lean-beam/deploy/beam-pool/tapis.py --env-file tapis.env status plan.json` prints the host; this needs a Tapis access token, but running the harness doesn't.
The pods read that token from the Tapis secret `infinileantoken`. If you only have a Tapis access token, you can recover the pool token from the secret without printing it:

    curl -sS -H "X-Tapis-Token: $(cat .secrets/TAPIS_TOKEN)" \
        https://tacc.tapis.io/v3/pods/secrets/infinileantoken/value |
      python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["secret_value"], end="")' \
        > .secrets/tapis-pool.token

    scripts/connect-tapis.sh
    BASE_URL=http://vista.tacc.utexas.edu:<port>/v1 API_KEY_FILE=/path/to/vista.key \
        scripts/run-tapis.sh runs/problem runs/mine

`connect-tapis.sh` starts the bridge and prints the pool's status; check that some workers are `"ready": true`.
`run-tapis.sh` is `run.sh` with `--pool-endpoint tapis-gateway:9000`, the Tapis token, and the Vista model as defaults. `BASE_URL` is required, because the Vista port changes whenever the deployment restarts.
Leave `RETRIEVAL` unset here; the supplied retrieval image is built for the local pool.

The sandbox image has to contain exactly the same prepared project as the Tapis workers, because attaching compares every file, compiled Mathlib included.
On an amd64 machine, `connect-tapis.sh` pulls the image the workers run (`debganguly/infinilean:0.0.1`, or `TAPIS_SOURCE_IMAGE`) and tags it as both `beam-pool:local` and `eureka-tapis:local`, with no local build.
`scripts/tapis-snapshot-diff.py` reports no differing files for it. To switch images later, delete both tags and `pool-packages-tapis/`, then rerun.

The workers run an amd64 image. Built on arm64, the same project has identical sources and `.olean` files, but 13 Lake `.trace` files record the platform and differ.
So on arm64, the first time it runs, `connect-tapis.sh` calls `build-tapis-image.sh`, which copies those files out of the Tapis image (`debganguly/infinilean:0.0.1`, without running it) onto `beam-pool:local`, checks that the result matches the snapshot exactly, and tags it `eureka-tapis:local`.
Its packages go to `pool-packages-tapis/`.
Rerun `build-tapis-image.sh`, and delete `pool-packages-tapis/`, whenever the Tapis deployment changes image.

Lean verifier still runs on this machine, so verification doesn't depend on Tapis.

## Running, stopping, and recovery

By default each model turn can produce up to 32,768 output tokens, reasoning included. Change this with `--max-output-tokens`.
Tool calls run one at a time, in the order the model made them. For parallel probes, use `lean_check {commands: [...]}`.

A lead can start helpers with `spawn_subagents`, which returns their attempt IDs straight away.
`wait_subagents {}` waits up to 30 seconds and returns the helpers that have finished since the last call, with their `finish` note and final file.
A lead can have at most 8 helpers it hasn't collected yet, and its file has to elaborate before it can spawn any (sorries are fine).

Agents are cancelled when their lead finishes, when a proof is published, or when the harness gets SIGINT or SIGTERM.
The harness then waits for each of them to clean up. Use SIGTERM to stop a run; SIGKILL skips the cleanup.

Each attempt saves its files under `<pool>/attempts/<id>/`, which no sandbox can see:

- `candidate.lean`: the target file, captured after the sandbox was stopped.
- `result.json`: label, status, termination reason, notes, token usage and the candidate path.
- `verification.txt`: the verifier's reports. If verification fails for operational reasons (a crash rather than a rejected proof), it is retried once and both reports are kept. Rejected proofs are not retried.

To continue from a saved file, start a new run with the same template and `--seed /path/to/candidate.lean`.
This restores the file, not the conversation.
To re-check a saved file without a model, run with `--verify-only /path/to/candidate.lean` and the usual template, store, packages arguments.
Either way the harness creates a new pool and leaves the old one alone.

Capture is on by default. Each attempt saves `request-NNNN.json`, `response-NNNN.json` and `stream-NNNN.jsonl` (never authorization headers).
The response includes the provider's terminal response, with reasoning and any logprobs it returned. Stream files also retain partial output and failed tries.
Output-text token logprobs are required by default. Unsupported requests or responses with missing scores fail the attempt after preserving the available data. There is no automatic fallback.
Some providers supply scores only for visible text; this does not promise probabilities for reasoning or tool-call tokens. A reasoning/tool-only response with no scored output text also fails the strict check.
Use `--logprobs=false` to disable logprob requests. `--capture-requests=false` explicitly disables capture; otherwise allow disk space for growing conversation histories.
The trace always records model and tool starts and finishes, retries and errors.
To summarize a trace, live or finished:

    python3 harness/trace_report.py /path/to/trace.jsonl
    python3 harness/trace_report.py /path/to/trace.jsonl --json

## Extra MCP servers

Add stdio MCP servers with `--mcp server.json`, once per server. For example, `tools/retrieval/mcp.json` is:

    {"name": "retrieval", "command": "/opt/retrieval/venv/bin/python", "args": ["/opt/retrieval/server.py", "--endpoint", "http://retrieval:8000"]}

The server runs inside each sandbox, so its command has to exist in the sandbox image, and it has the same filesystem and network limits.
Its tools appear to the model as `mcp_<name>_<tool>`.
For local Loogle and hosted lemma search, see the [retrieval tools](../../tools/retrieval/README.md).

## Integration tests

These use real Docker, Beam and Lean verifier with scripted model responses, so no provider is called.
Build the harness first, then run:

    HARNESS_PACKAGES=/path/to/pool-packages \
    HARNESS_POOL_TOKEN_FILE=/path/to/pool.token \
    python3 harness/tests/lifecycle.py -v
