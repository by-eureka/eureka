#!/usr/bin/env python3
"""Prepare selected VeriSoftBench proof holes in their original Lean source files."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib

BENCHMARK = "8f5e8b36ab5fbdec6fd8d8f4ee0580d29647097a"
EXAMPLES = Path(__file__).resolve().parents[1] / "examples"


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args])


def remove_proof(source, task):
    # Match both pieces byte-for-byte, apart from whitespace at the := boundary.
    # Guessing a declaration boundary can leave the original answer in the project.
    statement = task["thm_stmt"].strip()
    proof = task["ground_truth_proof"].strip()
    if not statement or not proof.startswith(":="):
        raise ValueError(f"task {task['id']} lacks a separate := proof body")
    pattern = re.escape(statement) + r"\s*" + re.escape(proof)
    matches = list(re.finditer(pattern, source))
    if len(matches) != 1:
        raise ValueError(f"task {task['id']}: expected one exact statement and proof in the pinned source")
    match = matches[0]
    return source[:match.start()] + statement + " := by sorry" + source[match.end():]


def prepare(bench, repo, ids, output):
    if output.exists():
        raise ValueError(f"destination already exists: {output}")
    if not ids or len(ids) != len(set(ids)):
        raise ValueError("select distinct task IDs")
    rows = {str(row["id"]): row for row in map(json.loads, git(
        bench, "show", f"{BENCHMARK}:data/verisoftbench.jsonl").splitlines())}
    tasks = [rows[i] for i in ids]
    projects = {t["lean_root"] for t in tasks}
    if len(projects) != 1:
        raise ValueError("one project per template; select tasks from the same repository")
    registry = json.loads(git(bench, "show", f"{BENCHMARK}:repos.json"))["repos"]
    commit = registry[projects.pop()]["commit"]
    lake = tomllib.loads(git(repo, "show", f"{commit}:lakefile.toml").decode())
    libs = [lib["name"] for lib in lake.get("lean_lib", [])]
    if (not libs or lake.get("srcDir", ".") != "." or any(
            lib.get("srcDir", ".") != "." for lib in lake["lean_lib"])):
        raise ValueError("this preparer expects root-layout Lean libraries in lakefile.toml")
    if "Proofs" in libs:
        raise ValueError("Proofs is reserved for the comparator modules")
    # Use the same pinned package snapshot as the stock Docker and Tapis images,
    # including for projects that import only Lean core.
    base = EXAMPLES / "simple"
    manifest = json.loads((base / "lake-manifest.json").read_text())
    available = {pkg["name"] for pkg in manifest["packages"]}
    missing = {req["name"] for req in lake.get("require", [])} - available
    if missing:
        raise ValueError(f"dev runtime lacks these project dependencies: {sorted(missing)}")

    # Only the pinned libraries and their notices enter the attempt. No Git
    # history, benchmark JSON, original oleans, examples or model answers.
    files = {}
    upstream_toolchain = None
    with subprocess.Popen(["git", "-C", str(repo), "archive", "--format=tar", commit],
                          stdout=subprocess.PIPE) as process:
        with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
            for member in archive:
                name = member.name
                if name == "lean-toolchain" and member.isfile():
                    upstream_toolchain = archive.extractfile(member).read().decode().strip()
                in_library = any(name == lib + ".lean" or name.startswith(lib + "/") for lib in libs)
                notice = "/" not in name and name.upper().startswith(("LICENSE", "COPYRIGHT", "NOTICE"))
                if member.isfile() and ((in_library and name.endswith(".lean")) or notice):
                    files[name] = archive.extractfile(member).read().decode()
        # Drain tar padding before waiting on git, including on small pipe buffers.
        while process.stdout.read(65536):
            pass
        if process.wait():
            raise ValueError(f"could not read pinned source archive {commit}")
    for task in tasks:
        path = task["rel_path"]
        if path not in files:
            raise ValueError(f"task {task['id']} is outside the prepared Lean libraries: {path}")
        files[path] = remove_proof(files[path], task)
    paths = list(dict.fromkeys(t["rel_path"] for t in tasks))
    imports = "".join("import " + p[:-5].replace("/", ".") + "\n" for p in paths)
    files["Proofs/Challenge.lean"] = imports
    active_toolchain = (base / "lean-toolchain").read_text().strip()
    if upstream_toolchain and upstream_toolchain != active_toolchain:
        selector = " ".join(upstream_toolchain.splitlines())
        files["Proofs/Challenge.lean"] = (
            f"-- VeriSoftBench replays these proofs on {selector}. This workspace uses {active_toolchain}.\n"
            "-- Prefer lemma names and tactics already used by the original modules.\n" + imports
        )
    files["Proofs/Solution.lean"] = imports
    files["Proofs.lean"] = "import Proofs.Solution\n"
    files["proof-files.json"] = json.dumps(paths, indent=2) + "\n"
    files["config.json"] = json.dumps({
        "challenge_module": "Proofs.Challenge", "solution_module": "Proofs.Solution",
        "theorem_names": [task["thm_name"] for task in tasks],
        "permitted_axioms": ["propext", "Quot.sound", "Classical.choice"],
    }, indent=2) + "\n"
    lakefile = (base / "lakefile.toml").read_text().splitlines(keepends=True)
    lakefile.insert(1, 'defaultTargets = ["Proofs"]\n')
    files["lakefile.toml"] = "".join(lakefile) + "".join(
        f'\n[[lean_lib]]\nname = {json.dumps(lib)}\n' for lib in libs)
    for name in ("lean-toolchain", "lake-manifest.json"):
        files[name] = (base / name).read_text()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        project = Path(tmp) / "project"
        for name, text in files.items():
            dest = project / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(text)
        project.rename(output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("benchmark", type=Path, help="utopia-group/VeriSoftBench checkout")
    parser.add_argument("repository", type=Path, help="checkout of the tasks' source repository")
    parser.add_argument("output", type=Path)
    parser.add_argument("--tasks", nargs="+", required=True, help="task IDs from the same repository")
    args = parser.parse_args()
    try:
        prepare(args.benchmark, args.repository, args.tasks, args.output)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"cannot prepare benchmark project: {error}\n")
