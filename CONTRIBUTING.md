# Development and stable versions

I keep two long-lived branches:

| Branch | Purpose | How changes land |
| --- | --- | --- |
| `main` | The checked stable version and repository default | A promotion or stable-fix PR, only after my explicit approval |
| `dev` | New features, toolchain upgrades and integration work | Feature PRs with passing CI |

The Beam/Lean migration belongs on `dev`. The stable baseline before this split
is tagged `stable-baseline-2026-11-15`, at commit
`d8afa2e729ae553d6d761494ae0ce03133ee85b3`.

`dev` is a development channel, not a scheduled daily release. Passing CI does
not automatically promote it to stable. Neither channel enables auto-merge.

## Separate checkouts

Keep the existing stable checkout and its services. Create a separate clone for
development so compiler packages, builds, submodule checkouts and run artifacts
remain independent:

```bash
git clone --branch dev --recurse-submodules https://github.com/by-eureka/eureka.git eureka-dev
cd eureka-dev
scripts/build.sh
scripts/setup-runtime.sh
```

For a new stable checkout, use `--branch main` and follow that branch's setup
instructions. Main retains its working runtime; do not copy dev's packages or
setup scripts into it. Keep old run directories with their matching checkout.

Development defaults:

| Resource | Default |
| --- | --- |
| Lean/Beam image | `eureka-lean:dev` |
| Agent image with retrieval | `eureka-agent:dev` |
| Retrieval service image | `eureka-retrieval:dev` |
| Prepared dependencies | `lean-packages-dev/` within the dev checkout |
| Retrieval Compose project | `eureka-retrieval-dev` |
| Internal agent network | `eureka-dev-agents` |

`scripts/stop-services.sh` stops the development retrieval project only.
Image tags identify the channel and can change when rebuilt; the harness pins
each run to an immutable image ID. Rebuild into a new `PACKAGES` directory when
upgrading dependencies. Never replace packages while a run uses them.

## Everyday work

Start feature branches from the current `dev`, use issues for concrete work,
and target PRs at `dev` explicitly:

```bash
git fetch origin
git switch -c feature/my-change origin/dev
# Make the change and run its relevant tests.
git push -u origin feature/my-change
gh pr create --base dev
```

Use a separate worktree if the current checkout has unfinished changes. The
root scripts are shared by both runtime setup and execution; test setup, run
and shutdown together when changing their defaults.

The required `checks` CI job combines the regression suite and native Beam /
verifier matrix. Both branches require PRs and block direct pushes, force
pushes and deletion, including administrator pushes. Dev additionally requires
`checks` to pass with the branch up to date. Main does not inherit dev's native
CI requirements until its runtime is promoted.

## Promotion to stable

Prepare a `dev` → `main` PR only when the development version is ready. Include
the exact commit, relevant CI and real-runtime results, migration instructions,
and any compatibility changes. Wait for my explicit approval of that promotion
before merging. An approval to integrate into `dev` does not authorize a merge
to `main`.

GitHub's PR requirement prevents direct pushes. The approval policy is a
separate instruction: do not infer my approval from green CI, repository access,
or the ability to press Merge. There is no automatic stable promotion job.

Stable fixes follow the same approval rule and should also be carried into
`dev`. Keep release history and the baseline tag so an earlier working version
remains available.
