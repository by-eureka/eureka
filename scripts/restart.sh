#!/usr/bin/env bash
# Continue a stopped run from its saved proof. The conversation starts fresh.
#
#   scripts/restart.sh runs/old runs/new [attempt-000001] [extra harness flags...]
#
# Use the same environment settings as run.sh. Stop a live run with stop.sh first.
# The default is the first lead. Select another attempt explicitly if needed.
set -euo pipefail
source "$(dirname "$0")/env.sh"
need_harness

if [ $# -lt 2 ]; then
    sed -n '4p' "$0" | sed 's/^#  *//' >&2
    exit 1
fi

old_run=$1
new_run=$2
shift 2
attempt=attempt-000001
if [[ $# -gt 0 && $1 != --* ]]; then
    attempt=$1
    shift
fi
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
python3 - "$old_run" "$new_run" "$attempt" "$scratch" <<'PY'
import hashlib
import json
from pathlib import Path
import re
import sys

old, new, attempt, scratch = sys.argv[1:]
old, new, scratch = Path(old).resolve(), Path(new).resolve(), Path(scratch)
if new.exists():
    sys.exit(f"{new} already exists; use a new run directory")
if not re.fullmatch(r"attempt-\d+", attempt):
    sys.exit("expected an attempt ID, such as attempt-000001")
pidfile = old / "pid"
if pidfile.exists():
    pid = int(pidfile.read_text().strip())
    if pid <= 1:
        sys.exit(f"invalid PID in {pidfile}")
    try:
        proc = Path(f"/proc/{pid}")
        args = (proc / "cmdline").read_text().split('\0')
        stores = [value for flag, value in zip(args, args[1:]) if flag == '--store']
        # A dead run can leave a zombie or a PID now owned by another process.
        running = Path(args[0]).name == 'harness' and any(
            (proc / 'cwd' / store).resolve() == old / 'store' for store in stores
        )
    except FileNotFoundError:
        pass
    else:
        if running:
            sys.exit(f"{old} is still running; stop it with scripts/stop.sh first")
pools = list((old / "store").glob("pool-*"))
if len(pools) != 1:
    sys.exit(f"expected one pool in {old}/store; found {len(pools)}")
template = pools[0] / "source"
artifacts = pools[0] / "attempts" / attempt
manifest = template / "proof-files.json"
paths = json.loads(manifest.read_text()) if manifest.is_file() else []
multi = len(paths) > 1
candidate = artifacts / ("candidate" if multi else "candidate.lean")
if not candidate.exists() or not (artifacts / "result.json").is_file():
    sys.exit(f"no completed attempt with a saved candidate at {artifacts}")
result = json.loads((artifacts / "result.json").read_text())
report = artifacts / "verification.txt"
feedback = report.read_text() if report.exists() else result.get("detail") or ""
handoff = result.get("note") or result.get("last_message") or ""
prompt = (
    "Continue the saved proof files. Read them and the trusted challenge, "
    "then check the current Lean diagnostics. Preserve the trusted definitions and theorem "
    "statements, except intentional answer holes listed in config.json's definition_names. "
    "Finish all remaining obligations. Do not give up or end with a partial summary. "
    "Submit only when the whole file has no errors or sorries and the target theorems use "
    "only the permitted axioms. Work from the supplied statements, saved progress and "
    "mathematical libraries; do not look up existing contest solutions. Answer definitions "
    "must give explicit answers, not restate the question. This is a fresh conversation. "
    "The previous agent's handoff may contain unverified claims.\n"
)
# Keep long reports below Linux's per-argument limit when passed through --prompt.
prompt += "\nPrevious handoff (last 12000 characters):\n" + handoff[-12000:]
prompt += "\n\nVerifier feedback (last 12000 characters):\n" + feedback[-12000:]
summaries = list(artifacts.glob("compaction-*/handoff.txt"))
if summaries:
    latest = max(summaries, key=lambda p: int(p.parent.name.removeprefix("compaction-")))
    # Keep the combined prompt below Linux's per-argument byte limit, including Unicode.
    prompt += ("\n\nLast compaction handoff (last 4000 characters; unverified):\n"
               + latest.read_text()[-4000:]
               + "\nHelpers from the previous run have stopped; their old IDs are no longer active.\n")
# Candidates are frozen after all tools stop, including edits made through shell.
# The last write_file event alone may be older than that final state.
metadata = {
    "previous_run": str(old), "attempt": attempt,
    "previous_status": result.get("status"), "template": str(template),
    "seed": str(candidate),
    "prompt": prompt,
}
if multi:
    metadata["seed_files_sha256"] = {name: hashlib.sha256((candidate / name).read_bytes()).hexdigest()
                                     for name in paths}
else:
    metadata["seed_sha256"] = hashlib.sha256(candidate.read_bytes()).hexdigest()
for name, value in {"template": str(template), "seed": str(candidate),
                    "run": str(new), "prompt": prompt}.items():
    (scratch / name).write_text(value)
(scratch / "restart.json").write_text(json.dumps(metadata, indent=2) + "\n")
PY

# run.sh supplies --dont-give-up and the usual model, runtime and retrieval settings.
run=$(cat "$scratch/run")
"$ROOT/scripts/run.sh" "$(cat "$scratch/template")" "$run" \
    --seed "$(cat "$scratch/seed")" --prompt "$(cat "$scratch/prompt")" "$@"
cp "$scratch/restart.json" "$run/restart.json"
