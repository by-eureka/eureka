"""Bind a new sandbox to the pool by reusing an earlier sandbox's attachment.

    python3 reattach.py --root R --endpoint H:P --config C --binding B --inputs I

B is the binding the first `beam_pool attach` of this run wrote, and I its input
manifest. Every sandbox mounts the same prepared files, so instead of hashing all
of them again this checks that the root holds exactly the manifest's input files,
with the same size and mtime. Files whose size or mtime differ (the template files,
which each attempt writes fresh) are hashed and must match the recorded digest.
The pool must still serve the binding's snapshot from a ready worker.
On success it writes C and its manifest as `beam_pool attach` does and prints the
same kind of JSON. Any mismatch exits non-zero, and the harness attaches in full.

Runs in the sandbox with PYTHONPATH pointing at Beam, next to scripts.beam_pool.

Why Python in a Rust harness: it imports Beam's own input filter, excluded
directories and digest function from the image, so which files count can't drift
from what `beam_pool attach` and the Beam runtime use. That is the only strong
reason. Running in the sandbox is not essential: the host sees the same files
through the bind mounts, and only the `info` call needs the agents network.
It depends on scripts.beam_pool internals, not a public API; if a new image
changes them this fails and the harness falls back to a full attach.
A Rust port on the host would be faster (parallel walk) and type-checked, but
needs its own copy of Beam's filter and a test comparing it with `input_file`
over the whole package tree to catch drift.
"""

import argparse
import asyncio
import hashlib
import json
import os
import sys
import uuid
from pathlib import Path

from scripts.beam_pool.__main__ import endpoint
from scripts.beam_pool.protocol import EXCLUDED_DIRS, digest_file, input_file
from scripts.beam_pool.transport import rpc


def fail(message: str) -> None:
    print(message, file=sys.stderr)
    sys.exit(1)


def present_inputs(root: Path) -> set[str]:
    # The same walk, filter and symlink check as Snapshot.create, on strings:
    # pathlib's relative_to dominates the time over Mathlib's ~80k files.
    top = str(root)
    found = set()
    for directory, dirs, names in os.walk(top):
        dirs[:] = [d for d in dirs if d not in EXCLUDED_DIRS]
        prefix = os.path.relpath(directory, top) + "/" if directory != top else ""
        for name in names:
            relative = prefix + name
            if input_file(relative):
                path = os.path.join(directory, name)
                resolved = os.path.realpath(path)
                if not (resolved == top or resolved.startswith(top + "/")):
                    fail(f"snapshot contains an external symlink: {path}")
                found.add(relative)
    return found


def checked_inputs(root: Path, recorded: list[dict]) -> list[dict]:
    inputs = []
    for entry in recorded:
        path = root / entry["path"]
        before = path.stat()
        sec, nsec = divmod(before.st_mtime_ns, 1_000_000_000)
        if (before.st_size, sec, nsec) == (entry["size"], entry["sec"], entry["nsec"]):
            inputs.append(entry)
            continue
        if digest_file(path) != entry["digest"]:
            fail(f"prepared file differs from the attached snapshot: {entry['path']}")
        after = path.stat()
        if (after.st_mtime_ns, after.st_size) != (before.st_mtime_ns, before.st_size):
            fail(f"project changed during attachment: {entry['path']}")
        inputs.append({"path": entry["path"], "size": before.st_size, "sec": sec, "nsec": nsec,
                       "digest": entry["digest"]})
    return inputs


def write_private(path: Path, data: bytes) -> None:
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with temporary.open("xb") as stream:
            os.chmod(temporary, 0o600)
            stream.write(data)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--endpoint", type=endpoint, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--binding", type=Path, required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    args = parser.parse_args()

    root = args.root.resolve(strict=True)
    binding = json.loads(args.binding.read_text())
    recorded = json.loads(args.inputs.read_bytes())
    if binding["root"] != str(root):
        fail("the earlier attachment was for a different root")
    if present_inputs(root) != {entry["path"] for entry in recorded}:
        fail("input files differ from the attached snapshot")
    inputs = checked_inputs(root, recorded)

    if (binding["host"], binding["port"]) != args.endpoint:
        fail("the earlier attachment used a different endpoint")
    info = asyncio.run(rpc(args.endpoint, os.environ.get("BEAM_POOL_TOKEN", ""), "info", {}))
    if binding["snapshot"] not in {w["snapshot"] for w in info["workers"] if w.get("ready")}:
        fail("no ready worker serves the attached snapshot")

    # The same layout attach.py writes: a manifest named by its hash, then the config.
    config = args.config.absolute()
    manifest = json.dumps(inputs, separators=(",", ":")).encode()
    inputs_name = config.name + "." + hashlib.sha256(manifest).hexdigest() + ".inputs.json"
    config.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    write_private(config.parent / inputs_name, manifest)
    data = {"schema": 2, "bindings": [{**binding, "binding": uuid.uuid4().hex, "inputs": inputs_name}]}
    write_private(config, (json.dumps(data, indent=2) + "\n").encode())
    print(json.dumps({"config": str(config), "root": str(root), "snapshot": binding["snapshot"],
                      "inputs": len(inputs), "reused": True}))


if __name__ == "__main__":
    main()
