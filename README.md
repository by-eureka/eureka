# Eureka

In development.

## Quick start

The `scripts/` directory wraps the steps below. From the repository root:

```bash
scripts/build.sh                    # Rust harness
scripts/setup-runtime.sh            # native Lean/Beam image and prepared packages
scripts/setup-retrieval.sh          # optional: Loogle and lemma search
scripts/new-problem.sh /path/to/Problem.lean runs/problem MyProblem.main_theorem
scripts/run.sh runs/problem runs/mine    # BASE_URL, MODEL, RETRIEVAL=1 as needed
scripts/watch.sh runs/mine
scripts/stop.sh runs/mine                # optional: stop early
scripts/restart.sh runs/mine runs/next   # continue a stopped run from its saved proof
scripts/reverify.sh runs/problem path/to/candidate.lean runs/recheck
scripts/stop-services.sh            # shut down optional retrieval
```

The default runtime uses Lean 4.35.0 and a matching Mathlib revision. Each attempt runs its own native Beam MCP server in an isolated container, with networking disabled unless retrieval is enabled. Prepared packages default to `lean-packages/`; set `PACKAGES` to choose a different path.

The old fork-specific worker pool and Tapis attachment protocol are retired. Use `BASE_URL` to select a remote model endpoint; Lean execution uses the configured Docker runtime. See [runtime setup and migration](harness/deploy/README.md).

Each script explains its arguments at the top. The rest of this README covers the same steps by hand.

Every run saves model requests, responses and stream events, including reasoning returned by the provider.
Output-text token logprobs are required by default. Missing or unsupported scores stop the run with an error.
Use `LOGPROBS=false` only to explicitly allow a run without them.
The files live under `store/pool-*/attempts/<id>/`. API authorization headers are never saved.

To rescore saved text with vLLM, use the full rendered prompt and answer:

```bash
BASE_URL=http://vista.tacc.utexas.edu:<port>/v1 MODEL=DeepSeek-V4.1-Flash \
  API_KEY_FILE=.secrets/vista.key \
  python3 scripts/score-logprobs.py rendered.txt --output scores.json
```

These are new scores, not recovered historical logprobs. Missing context or reasoning cannot be restored.
Use `--continuation answer.txt` to append an answer, or `--token-ids` to read a JSON array of exact token IDs.
No chat template is added. The first token normally has no score. Missing scores after it cause an error.

`restart.sh` keeps the saved proof, handoff notes and verifier feedback. The conversation starts fresh.
Use the same model and runtime environment settings as before. It enables `--dont-give-up` through `run.sh`.
It defaults to `attempt-000001`; pass another attempt ID as the third argument to resume that one.

Full contexts compact automatically. The prover writes a handoff and continues with the same files and tools.
Each compaction saves its history, proof snapshot and summary under the attempt directory. Original instructions and verification stay fixed.
`--dont-give-up` continues text-only replies. Use `MAX_TURNS=10000 scripts/run.sh ...` for longer runs. Compactions do not reset that limit or have a count limit.

## Setup

You need Linux, Docker, Rust 1.91+, Python 3.10+, and Lean 4.35+ (installed with elan).
Clone with `--recurse-submodules`, then set up the [native runtime](harness/deploy/README.md) for the selected toolchain.
Verification uses the toolchain’s bundled `lake comparator` and needs `bwrap` (Bubblewrap) and a working systemd user session. There is no separate verifier checkout or exporter to build.

Build the harness from the repository root:

```bash
cargo build --locked --manifest-path harness/Cargo.toml
```

## Setting up a problem

Start from the example template and keep its build files, so the Lean and Mathlib versions match the runtime image.
Your Lean file should have the imports, definitions and theorem statements, with `sorry` for every proof.
It goes in as both the challenge and the starting solution:

```bash
mkdir -p runs
cp -r examples/simple runs/problem
cp /path/to/Problem.lean runs/problem/Proofs/Challenge.lean
cp runs/problem/Proofs/Challenge.lean runs/problem/Proofs/Solution.lean
```

Set `theorem_names` in `runs/problem/config.json` to the fully qualified names, e.g. `MyProblem.main_theorem`.
Then read through `Challenge.lean` carefully. Those statements are exactly what the verifier checks.

## Running

Set the endpoint and model. On Vista, use the current deployment's URL and key; the port
changes whenever the deployment restarts, so the scripts have no default for it.
The API key is read from the terminal so it stays out of your shell history:

```bash
BASE_URL=http://vista.tacc.utexas.edu:<port>/v1
MODEL=DeepSeek-V4.1-Flash
read -rsp 'API key: ' DEEPSEEK_API_KEY
echo
export DEEPSEEK_API_KEY
```

Other providers work if they support the `/responses` API. For DeepSeek's own API, use `https://api.deepseek.com` and `deepseek-flash`.

Start the run, using the packages extracted from your runtime image.
Use a new `RUN` directory each time. The harness runs under `nohup`, so closing the terminal won't stop it:

```bash
RUN=runs/mine
mkdir -p "$RUN/store"
nohup harness/target/debug/harness \
  --template runs/problem --store "$RUN/store" \
  --packages /path/to/lean-packages \
  --base-url "$BASE_URL" --model "$MODEL" \
  --max-turns 200 --max-output-tokens 200000 --dont-give-up \
  --trace "$RUN/trace.jsonl" > "$RUN/run.log" 2>&1 &
echo $! > "$RUN/pid"
```

`--max-output-tokens` includes reasoning, and input plus output has to fit in the server's context window.
To give the agents Loogle and lemma search, set up the [retrieval tools](tools/retrieval/README.md) and add `--image eureka-agent:local --network harness-agents --mcp tools/retrieval/mcp.json`.

To stop a run early, use `scripts/stop.sh "$RUN"`. The agents save their work before the harness exits.

## Watching a run

In another terminal, with `RUN` set to the same directory:

```bash
RUN=runs/mine
scripts/watch.sh "$RUN"
```

This shows each agent's status, turns, token usage and tool errors.
The live dashboard needs Rich: `python3 -m pip install -r harness/requirements.txt`.
Pass several run directories to `scripts/watch.sh` to see them together. Ctrl-C closes only the monitor.
For a snapshot or JSON, use `python3 harness/trace_report.py "$RUN/trace.jsonl"` with optional `--json`.
A long model response can leave the trace quiet for a while until it finishes.
In the status column, `verified` means Lean’s bundled verifier accepted the proof, `finished` means a helper handed its work back to the lead, and `not_submitted` means the agent stopped without submitting anything.
