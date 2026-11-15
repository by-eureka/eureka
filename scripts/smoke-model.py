#!/usr/bin/env python3
"""Run two bounded model-driven proofs through Docker, Beam and independent verification.

Credentials come from DEEPSEEK_API_KEY or OPENAI_API_KEY. No credentials are saved.
DeepSeek tool-only responses do not always provide logprobs, so this smoke explicitly
opts out of scores. The harness default remains strict.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CASES = {
    "arithmetic": (
        "theorem add_comm_nat (a b : Nat) : a + b = b + a := by sorry\n"
        "theorem two_mul_eq (n : Nat) : 2 * n = n + n := by sorry\n",
        ["add_comm_nat", "two_mul_eq"],
    ),
    "mathlib": (
        "import Mathlib\n"
        "theorem square_nonnegative (x : ℝ) : 0 ≤ x ^ 2 := by sorry\n"
        "theorem reverse_twice (xs : List Nat) : xs.reverse.reverse = xs := by sorry\n",
        ["square_nonnegative", "reverse_twice"],
    ),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", type=Path, required=True)
    parser.add_argument("--template", type=Path, default=ROOT / "examples/simple",
                        help="trusted build files for the runtime being tested")
    parser.add_argument("--image", default="eureka-lean:local")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base-url", default="https://api.deepseek.com")
    parser.add_argument("--model", default="deepseek-flash")
    args = parser.parse_args()
    if not (os.getenv("DEEPSEEK_API_KEY") or os.getenv("OPENAI_API_KEY")):
        parser.error("set DEEPSEEK_API_KEY or OPENAI_API_KEY")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    results = []
    for name, (source, names) in CASES.items():
        run = output / name
        template = run / "template"
        (template / "Proofs").mkdir(parents=True)
        for filename in ("lakefile.toml", "lake-manifest.json", "lean-toolchain", "Proofs.lean"):
            shutil.copy2(args.template / filename, template / filename)
        for module in ("Challenge", "Solution"):
            (template / "Proofs" / f"{module}.lean").write_text(source)
        config = {"challenge_module": "Proofs.Challenge", "solution_module": "Proofs.Solution",
                  "theorem_names": names, "permitted_axioms": ["propext", "Quot.sound", "Classical.choice"]}
        (template / "config.json").write_text(json.dumps(config, indent=2) + "\n")
        (run / "store").mkdir()
        command = [str(ROOT / "harness/target/debug/harness"),
                   "--template", str(template), "--store", str(run / "store"),
                   "--packages", str(args.packages.resolve()), "--image", args.image,
                   "--beam-skill", str(ROOT / "lean-beam/skills/lean-beam"),
                   "--trace", str(run / "trace.jsonl"), "--base-url", args.base_url,
                   "--model", args.model, "--max-turns", "12", "--max-output-tokens", "8192",
                   "--effort", "low", "--logprobs=false"]
        with (run / "run.log").open("w") as log:
            child = subprocess.Popen(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
            try:
                code = child.wait(timeout=600)
            except subprocess.TimeoutExpired:
                child.terminate()  # The harness stops containers and saves candidates.
                try:
                    child.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
                    raise RuntimeError(f"cleanup did not settle; inspect {run}") from None
                code = 124
        attempts = [json.loads(path.read_text()) for path in run.glob("store/pool-*/attempts/*/result.json")]
        passed = code == 0 and any(row["status"] == "verified" for row in attempts)
        results.append({"case": name, "exit_code": code, "verified": passed,
                        "model": args.model, "base_url": args.base_url,
                        "logprobs": False, "artifacts": str(run)})
        (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(f"{name}: {'verified' if passed else 'FAILED'} ({run})", flush=True)
    if not all(row["verified"] for row in results):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
