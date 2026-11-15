# Migration validation — 2026-11-14

I tested the migration on review branches. Nothing has been merged to main.
The verifier changes are in [PR #10](https://github.com/by-eureka/eureka/pull/10);
the native runtime is stacked on them in [PR #11](https://github.com/by-eureka/eureka/pull/11).

## Versions

| Component | Revision |
| --- | --- |
| Beam | `leanprover/lean-beam@86d26a9a43dadf1e572d09b19c7ffa458c10b480` |
| Default Lean | `leanprover/lean4:v4.35.0` |
| Default Mathlib | `56a005855ea17e0b0958f73ec801ae140b0a1679` |
| Additional test toolchain | `leanprover/lean4:v4.36.0-rc2` |
| Release-candidate Mathlib | `9d581dfff2e41de989072ea63abf3daf69311520` |
| Model | `deepseek-flash` at `https://api.deepseek.com` |

The local Docker tests ran on Linux ARM64. GitHub's native test matrix runs on
Ubuntu x86-64. Archive checksums for both architectures are pinned in
`lean-releases.json`.

The [GitHub Actions run](https://github.com/by-eureka/eureka/actions/runs/43670650224)
passed all three jobs: regression, native Lean 4.35.0, and native Lean 4.36.0-rc2.

## Results

| Check | Result |
| --- | --- |
| Harness Rust unit / response streaming tests | 12 + 4 passed |
| Worktrees unit / integration / documentation tests | 3 + 26 + 4 passed |
| Harness Python regressions | 24 passed |
| Retrieval Python tests | 10 passed |
| Real Docker sandbox tests | 3 passed |
| Lifecycle with Docker, Beam and independent verification | 18 passed; optional retrieval case passed separately |
| Native Beam surface and verifier boundary, Lean 4.35.0 | Passed |
| Native Beam surface and verifier boundary, Lean 4.36.0-rc2 | Passed |
| Mismatched runtime compiler | Rejected before credential loading |
| Missing packages, wrong dependency revision, failed workspace setup | Rejected before model requests |

The Beam tests cover document versions, speculative checks, whole-file todo
discovery, term holes, profiling, handle invalidation and workspace recovery.
The verifier tests accept valid proofs and reject sorry, changed statements or
definitions, extra axioms, native assertions, syntax errors, package writes and
forged success output. Changes outside the candidate file are discarded.

The lifecycle suite also covers cancellation, child attempts, extra MCP servers,
failed submissions, saved-proof reverification, strict logprob capture, and
2,001 turns with twenty context compactions. Retrieval validation checks local
search and Loogle, including recovery after an invalid query. Hosted retrieval
providers are checked for tool registration only.

## Real DeepSeek proofs

I built the final image with `scripts/setup-runtime.sh`, extracted its packages,
and ran `scripts/smoke-model.py` against those packages. Both cases submitted and
passed independent kernel verification:

| Lean | Case | Theorems | Turns | Input tokens | Output tokens |
| --- | --- | --- | ---: | ---: | ---: |
| 4.35.0 | Arithmetic | `add_comm_nat`, `two_mul_eq` | 8 | 114,519 | 1,029 |
| 4.35.0 | Mathlib | `square_nonnegative`, `reverse_twice` | 6 | 82,821 | 510 |
| 4.36.0-rc2 | Arithmetic | `add_comm_nat`, `two_mul_eq` | 11 | 158,312 | 1,122 |
| 4.36.0-rc2 | Mathlib | `square_nonnegative`, `reverse_twice` | 11 | 162,227 | 1,618 |

The stable-run proofs use `Nat.add_comm`, `Nat.two_mul`, `positivity`, and `simp`.
I also built and extracted a separate 4.36.0-rc2 image with matching Mathlib and
repeated both model checks successfully. Local artifacts are under
`runs/migration-deepseek-final/` and `runs/migration-deepseek436/`: request/response
traces, candidates, verifier reports and `results.json`. Credentials are not
included. These bounded checks use at most 12 turns and 8,192 output tokens per
turn, with low reasoning effort and an explicit `--logprobs=false`.

## Fixes found during validation

- A normal Mathlib batch build did not prepare every hash needed by the Lean
  server. Read-only dependencies then caused workspace setup to fail. The image
  now runs `lake setup-file` on an `import Mathlib` fixture before extraction.
  A real lifecycle regression covers this failure.
- Native Beam requires the project's toolchain selector to resolve to its own
  SDK. The image supplies that resolution through its private elan directory.
- Beam 0.2 puts common workflow rules in MCP server instructions and rejects the
  old `lean_todo` suggestion argument. The harness supplies the instructions and
  tests the current tool schemas.

## Reproducing the checks

The [runtime guide](README.md#tests) lists the Rust, Python, native and lifecycle
commands. For the model checks, export a provider key in the environment and run:

```bash
IMAGE=eureka-lean:local PACKAGES="$PWD/lean-packages" scripts/setup-runtime.sh
python3 scripts/smoke-model.py --image eureka-lean:local \
  --packages "$PWD/lean-packages" --output runs/model-smoke
```

Use a new output directory for each run. To check another toolchain, prepare its
matching template, image and packages, then pass `--template /path/to/template`
as well. Keep the compiler and Mathlib pins together.

## Limits

DeepSeek tool-only replies can omit logprobs, so these proof checks explicitly
opt out of them. The harness default still rejects missing scores. This does
not validate logprob availability on every provider.

Lean 4.35's exporter can panic on a missing configured declaration. That remains
an operational failure, with a bounded retry and no publication. The retired
fork's distributed workers and Tapis attachment protocol are not restored by
the native migration. Existing proof sources can be ported to a fresh template.
