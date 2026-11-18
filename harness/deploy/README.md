# Native Lean and Beam runtime

By default the harness starts a local Docker container for each attempt. For remote execution it can create a [TACC pod per attempt](#tacc-proof-pods). Each container runs its own Beam MCP server and has a private writable Lean project with read-only prepared dependencies. Model credentials stay on the controller. Local Docker containers use `--network none` by default.

This runtime belongs to the `dev` channel. Keep it in a separate checkout from
stable `main`; see [development and promotion](../../CONTRIBUTING.md).

## Setup

Install Rust 1.91+, Python 3.10+, Docker, elan, Bubblewrap, and a working systemd user session. The template's Lean toolchain must be installed on the host for independent verification.

```bash
elan toolchain install leanprover/lean4:v4.35.0
git submodule update --init --recursive
scripts/build.sh
scripts/setup-runtime.sh
```

`setup-runtime.sh` builds `eureka-lean:dev` from the pinned Beam submodule revision, verifies the downloaded Lean release checksum, prepares the pinned Mathlib cache, and extracts packages into `lean-packages-dev/`. Set `IMAGE` and `PACKAGES` to use different names. Package extraction publishes the directory only after it completes; an existing destination is never overwritten.

The default template, image and packages must be upgraded together. Before calling the model, the harness checks package git revisions and compares the image's compiler commit with the template's host compiler. It then uses the image's immutable ID for the run.

The supported test matrix is Lean 4.35.0 and 4.36.0-rc2. Templates default to the stable 4.35 release. To upgrade, change their `lean-toolchain`, pin the corresponding Mathlib revision, regenerate the manifests, and rebuild/extract with `setup-runtime.sh`. The script selects the compiler from `examples/simple/lean-toolchain`; release checksums for both architectures are recorded in `lean-releases.json`. New releases need their official archive digests added there.

```bash
scripts/new-problem.sh /path/to/Problem.lean runs/problem Namespace.theorem_name
scripts/run.sh runs/problem runs/proof
scripts/watch.sh runs/proof
scripts/stop.sh runs/proof
scripts/restart.sh runs/proof runs/continued
scripts/reverify.sh runs/problem /path/to/candidate.lean runs/recheck
```

`BASE_URL`, `MODEL`, `API_KEY_FILE`, `MAX_TURNS`, and `MAX_OUTPUT_TOKENS` configure model runs. Reverification needs neither an API key nor Docker.

## Verification

Lean 4.35+ supplies `lake comparator --config config.json`, its matching exporter and kernel checker. The separate Comparator checkout, `--comparator` flag and Landrun dependency are removed. Lake uses Bubblewrap to build and export untrusted code; systemd bounds the entire process tree. Shared packages are mounted read-only, and verification starts from a fresh trusted template with only the candidate source substituted.

A successful compile or speculative probe is not a verified proof. Publication requires matching statements and dependencies, permitted axioms, and independent kernel acceptance. There is no custom `allow_bv_decide` exemption: native assertions must satisfy the bundled verifier's axiom policy. Old `enable_nanoda` configurations should use `external_kernels` instead.

Lean 4.35's exporter can panic when a named declaration is missing. The harness records that as an operational failure, retries once, and never publishes the candidate. Known proof rejections remain distinct from infrastructure failures.

## Optional retrieval

```bash
scripts/setup-retrieval.sh
RETRIEVAL=1 scripts/run.sh runs/problem runs/with-search
scripts/stop-services.sh
```

Retrieval uses the `eureka-retrieval-dev` Compose project, `eureka-retrieval:dev`
service image, and internal `eureka-dev-agents` network. Only the retrieval
service has outbound access; attempt containers do not. Custom networks must
preserve that boundary. These defaults keep the stable services separate.
See [retrieval setup](../../tools/retrieval/README.md).

## Migrating existing runs

The former Beam fork's gateway, remote workers and Tapis attachment format are not supported by current upstream Beam. The corresponding setup and bridge scripts are removed. Existing services and run directories are not changed by this migration. Stop old runs using their original checkout before retiring those services.

Old proof sources and handoff notes can be reused, but their Lean 4.33 project manifests and prepared packages cannot. Create a new template with the current build files, port the trusted challenge and candidate, and pass `--seed /path/to/candidate.lean` for a new run. Keep the original theorem statements intact. Run the same harness on a Docker-capable remote host when execution needs to happen there; `BASE_URL` independently selects a remote model provider.

## Tests

```bash
cargo test --locked --manifest-path harness/Cargo.toml --lib --test responses
python3 -m unittest discover -s harness/tests -p 'test_*.py'
cargo test --locked --manifest-path worktrees/Cargo.toml
cargo test --locked --manifest-path harness/Cargo.toml --test lake_verifier -- --ignored --nocapture
HARNESS_IMAGE=eureka-lean:dev cargo test --locked --manifest-path harness/Cargo.toml --test sandbox -- --ignored --nocapture
HARNESS_PACKAGES="$PWD/lean-packages-dev" HARNESS_IMAGE=eureka-lean:dev python3 harness/tests/lifecycle.py -v
```

The lifecycle suite uses scripted HTTP responses with real Docker, Beam and verification. The optional retrieval case runs only when `HARNESS_RETRIEVAL_IMAGE` is set and its service is running. To test a host native Beam installation, set `HARNESS_BEAM_MCP` to its executable and `HARNESS_PACKAGES` to matching dependencies, then run `cargo test --locked --manifest-path harness/Cargo.toml --test beam_surface -- --ignored --nocapture`. Its `lean-toolchain` selector must resolve to that installation through elan.

For a bounded real-model check, set `DEEPSEEK_API_KEY` in the environment and run:

```bash
python3 scripts/smoke-model.py --packages "$PWD/lean-packages-dev" --output runs/model-smoke
```

This checks arithmetic and Mathlib proofs with `deepseek-flash`, at most 12 turns each. It explicitly disables logprobs because DeepSeek tool-only responses can omit them. Normal runs retain the strict logprob default. Results and proof artifacts are saved under the chosen output directory; credentials are not saved.

Pass `--template /path/to/template` with the corresponding image and packages to check another compiler. See [recorded migration validation](VALIDATION.md) for versions, outcomes and limits.

## TACC proof pods

The Rust controller and independent verifier run on the operator's machine. Each lead and each helper gets its own TACC pod. The pod runs the upstream Beam MCP server, shell commands and a private Lean workspace. The controller opens an SSH command channel for each MCP server and keeps it open across tool calls. Tapis routes this SSH transport using a TLS hostname on port 443. The controller learns the fresh SSH host key through the authenticated Tapis control API. Only its per-attempt public key is mounted in the pod.

Build a new x86 image from a dev checkout with its pinned submodule. Choose an unused release tag in the allowlisted Docker Hub repository.

```bash
export BEAM_REV="$(git -C lean-beam rev-parse HEAD)"
export IMAGE=debganguly/infinilean:YOUR_NEW_TAG
docker buildx bake -f harness/deploy/tapis/bake.hcl pod --load
docker push "$IMAGE"
```

The build contexts admit only the dev template, the SDK Dockerfile and the pod entrypoint. The worker runs as an unprivileged user and its prepared Lean and Mathlib files are owned by root. Proof commands inherit a syscall filter that prevents opening internet sockets. The optional Docker retrieval network is available with the Docker backend.

On the controller, prepare the matching host packages and verifier as described above. The transport also requires Python 3, OpenSSH client tools, OpenSSL, tar, and API access to the TACC Pods service. The two credential files should be private to the controller account.

```bash
export TAPIS_TOKEN_FILE=/private/path/tacc-access.token
export API_KEY_FILE=/private/path/deepseek.key
export IMAGE=debganguly/infinilean:YOUR_RELEASE_TAG
export LOGPROBS=false  # DeepSeek can omit probabilities for tool calls
scripts/run.sh examples/simple runs/tapis-example --tapis-backend "$PWD/scripts/tapis-pod.py"
```

For independent problems, start one run per template with a distinct output directory. Pass `--agents 2` to give two leads the same problem. Each lead or helper creates a separate pod and uses one pod for its whole attempt, including every Beam call. A helper receives a copy of its parent's current target and returns its candidate and notes through the controller. Choose concurrency and per-pod `--cpus` and `--memory-mib` for your TACC allocation.

Each attempt records its pod ID and runtime identity under its host artifact directory in `tapis/`. The controller captures the candidate, deletes the pod, and verifies the captured bytes against a fresh trusted template. Helpers return their candidate to the lead. A completed controller cleanup leaves a `deleted` marker. If the controller is lost, `TAPIS_TIME_TO_STOP` bounds the pod lifetime from startup (default 3600 seconds). Remove an orphan from its recorded attempt directory with `scripts/tapis-pod.py delete /path/to/attempt/tapis`. An expired TACC token must be refreshed for cleanup. A pod timeout or loss of its ephemeral storage can lose edits that have not yet been returned.
