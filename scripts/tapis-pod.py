#!/usr/bin/env python3
"""TACC Pods lifecycle for the Eureka controller. The model never sees these credentials."""
import argparse
import base64
from contextlib import contextmanager
import fcntl
import http.client
import json
import os
from pathlib import Path
import random
import re
import secrets
import subprocess
import ssl
import sys
import tempfile
import time
import urllib.error
import urllib.request

API = "https://tacc.tapis.io/v3/pods"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


class TransientError(RuntimeError):
    pass


class ConflictError(RuntimeError):
    pass


class MissingIngressRoute(RuntimeError):
    pass


def seconds_left(deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("Tapis lifecycle deadline exceeded")
    return remaining


def backoff(deadline, delay):
    time.sleep(min(seconds_left(deadline), random.uniform(delay * 0.8, delay * 1.2)))


@contextmanager
def startup_slot():
    # All harness processes on the controller share these locks. A process exit releases its slot.
    limit = int(os.getenv("TAPIS_MAX_STARTING", "3"))
    if limit < 1:
        raise ValueError("TAPIS_MAX_STARTING must be positive")
    cache = Path(os.getenv("XDG_CACHE_HOME") or Path.home() / ".cache") / "eureka" / "tapis-starts"
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    while True:
        for index in range(limit):
            with (cache / str(index)).open("a") as slot:
                try:
                    fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    continue
                yield
                return
        time.sleep(random.uniform(1, 3))


def request(method, path, payload, timeout):
    token_file = os.getenv("TAPIS_TOKEN_FILE")
    if not token_file:
        raise ValueError("set TAPIS_TOKEN_FILE to a private TACC access token file")
    token = Path(token_file).read_text().strip()
    if not token:
        raise ValueError("empty Tapis token file")
    req = urllib.request.Request(API + path, method=method,
        data=None if payload is None else json.dumps(payload).encode(),
        headers={"X-Tapis-Token": token, "Content-Type": "application/json"})
    try:
        with urllib.request.build_opener(NoRedirect()).open(req, timeout=timeout) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404 and method in {"GET", "DELETE"}:
            return None
        try:
            detail = str(json.load(error).get("message", "")).replace(token, "[redacted]")
            detail = re.sub(r"eyJ[\w.-]{20,}", "[redacted]", detail)[:450]
        except (ValueError, AttributeError):
            detail = ""
        message = f"Tapis {method} {path}: HTTP {error.code}: {detail}"
        if error.code in {408, 429, 500, 502, 503, 504}:
            raise TransientError(message) from None
        if error.code == 409:
            raise ConflictError(message) from None
        raise RuntimeError(message) from None
    except (urllib.error.URLError, ConnectionError, TimeoutError, http.client.IncompleteRead, json.JSONDecodeError) as error:
        if isinstance(getattr(error, "reason", None), ssl.SSLCertVerificationError):
            raise RuntimeError("Tapis TLS certificate verification failed") from None
        raise TransientError(f"Tapis {method} {path}: {type(error).__name__}") from None
    if result.get("status") != "success":
        raise RuntimeError(f"Tapis {method} {path} failed")
    return result["result"]


HOST_KEY = {"commands": [["cat", "/run/eureka/host.pub"]], "fail_on_non_success": False}


def api(method, path, payload=None, *, deadline):
    delay = 2
    while True:
        try:
            return request(method, path, payload, min(90, seconds_left(deadline)))
        except TransientError:
            if method != "GET" and not (path.endswith("/exec") and payload == HOST_KEY):
                raise
            backoff(deadline, delay)
            delay = min(20, delay * 2)


def owned(row, description):
    if row is not None and row.get("description") != description:
        raise RuntimeError("Tapis pod belongs to another attempt")
    return row


def submit(template, deadline):
    path = "/" + template["pod_id"]
    while True:
        try:
            api("POST", "", template, deadline=deadline)
            return
        except (ConflictError, TransientError):
            # The server can commit a create even when the caller receives a gateway error.
            # Every retry uses the original ID, public key and ownership marker.
            for delay in (2, 5, 12):
                backoff(deadline, delay)
                if owned(api("GET", path, deadline=deadline), template["description"]) is not None:
                    return


def ssh(state, args, *, diagnose_tls=False):
    meta = json.loads((state / "pod.json").read_text())
    host = meta["id"] + ".pods.tacc.tapis.io"
    encoded = base64.b64encode(json.dumps(args).encode()).decode()
    proxy = "openssl s_client -quiet -verify_return_error -verify_hostname %h -servername %h -connect %h:443"
    if not diagnose_tls:
        proxy += " 2>/dev/null"
    cmd = ["ssh", "-F", "/dev/null", "-T", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
           "-o", "IdentityAgent=none", "-o", "ClearAllForwardings=yes", "-o", "StrictHostKeyChecking=yes",
           "-o", "GlobalKnownHostsFile=/dev/null", "-o", "UserKnownHostsFile=" + str(state / "known_hosts"),
           "-o", "ConnectTimeout=12", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3",
           "-o", "ProxyCommand=" + proxy,
           "-i", str(state / "identity"), "prover@" + host, encoded]
    return cmd


def delete(state):
    with (state / "cleanup.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if (state / "deleted").exists():
            return
        meta = json.loads((state / "pod.json").read_text())
        path = "/" + meta["id"]
        deadline = time.monotonic() + 360
        next_delete = 0
        while True:
            row = owned(api("GET", path, deadline=deadline), meta["description"])
            if row is None:
                # An interrupted create can appear after the first successful lookup.
                if (state / "create-pending").exists():
                    backoff(deadline, 15)
                    if owned(api("GET", path, deadline=deadline), meta["description"]) is not None:
                        continue
                break
            if time.monotonic() >= next_delete:
                try:
                    api("DELETE", path, deadline=deadline)
                except (TransientError, ConflictError):
                    pass  # Look up this same owned pod before retrying a mutation.
                next_delete = time.monotonic() + 30
                continue
            backoff(deadline, 4)
        (state / "identity").unlink(missing_ok=True)
        (state / "deleted").touch()


def create(state, root, image, memory, cpus):
    deadline = time.monotonic() + int(os.getenv("TAPIS_START_TIMEOUT", "900"))
    try:
        create_once(state, root, image, memory, cpus, deadline)
    except MissingIngressRoute:
        # A new route needs a new pod ID. Retain the first receipt and never overlap pods.
        if not (state / "deleted").is_file() or time.monotonic() >= deadline:
            raise
        state.rename(state.with_name(state.name + "-unrouted"))
        create_once(state, root, image, memory, cpus, deadline)


def missing_ingress(test):
    return (test is not None and test.returncode == 255
            and b"TRAEFIK DEFAULT CERT" in test.stderr
            and b"self-signed certificate" in test.stderr)


def create_once(state, root, image, memory, cpus, deadline):
    if not re.fullmatch(r"debganguly/infinilean:[A-Za-z0-9_.-]+", image) or image.endswith(":latest"):
        raise ValueError("expected a versioned debganguly/infinilean image tag")
    state.mkdir(mode=0o700)
    ident = "eur" + secrets.token_hex(8)
    description = "Eureka attempt " + secrets.token_hex(16)
    (state / "pod.json").write_text(json.dumps({"id": ident, "description": description, "image": image}) + "\n")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-C", "", "-N", "", "-f", str(state/"identity")], check=True)
    key = " ".join((state / "identity.pub").read_text().split()[:2])
    resources = {"cpu_request": cpus, "cpu_limit": cpus, "mem_request": memory, "mem_limit": memory}
    if scratch := os.getenv("TAPIS_SCRATCH_MIB"):
        resources["ephemeral_storage_request"] = int(scratch)
        resources["ephemeral_storage_limit"] = int(scratch)
    template = {"pod_id": ident, "description": description, "image": image,
                "volume_mounts": {"/etc/ssh/eureka_authorized_keys": {
                    "type": "ephemeral", "config_content": "restrict " + key + "\n",
                    "config_permissions": "0444", "config_update_mode": "once"}},
                "resources": resources,
                "networking": {"default": {"protocol": "tcp", "port": 2222}},
                "time_to_stop_default": int(os.getenv("TAPIS_TIME_TO_STOP", "3600"))}
    path = "/" + ident
    started = time.monotonic()
    last_status = None
    try:
        seconds_left(deadline)
        pending = state / "create-pending"
        pending.touch()
        submit(template, deadline)
        pending.unlink()
        next_attestation = 0
        missing_since = None
        while time.monotonic() < deadline:
            row = owned(api("GET", path, deadline=deadline), description)
            status = (row or {}).get("status")
            container = (row or {}).get("status_container", {})
            progress = (status, container.get("phase"), container.get("message"))
            if progress != last_status:
                with (state / "startup.jsonl").open("a") as log:
                    log.write(json.dumps(dict(seconds=round(time.monotonic() - started, 2),
                                              status=status, phase=progress[1], message=progress[2])) + "\n")
                last_status = progress
            if status in {"STOPPED", "COMPLETE", "ERROR"}:
                raise RuntimeError(f"{ident} stopped during startup ({status}); see {state}/startup.jsonl")
            if status != "AVAILABLE":
                missing_since = None
            if status == "AVAILABLE":
                # Bootstrap the host key through the authenticated Tapis control plane.
                if time.monotonic() >= next_attestation:
                    result = api("POST", path + "/exec", HOST_KEY, deadline=deadline)
                    output = result.get("execution_results", []) if isinstance(result, dict) else result
                    if output and isinstance(output[0], dict):
                        public = output[0].get("stdout", "").strip().split()
                        if len(public) >= 2 and public[0] == "ssh-ed25519" and re.fullmatch(r"[A-Za-z0-9+/]+=*", public[1]):
                            (state / "known_hosts").write_text(f"{ident}.pods.tacc.tapis.io {public[0]} {public[1]}\n")
                            next_attestation = time.monotonic() + 60
                if (state / "known_hosts").is_file():
                    try:
                        test = subprocess.run(ssh(state, ["exec", "/opt/lean/bin/lean", "--githash"], diagnose_tls=True),
                                              capture_output=True, timeout=min(25, seconds_left(deadline)))
                    except subprocess.TimeoutExpired:
                        test = None
                    if test is not None and test.returncode == 0:
                        break
                    if missing_ingress(test):
                        now = time.monotonic()
                        if missing_since is None:
                            missing_since = now
                        elif now - missing_since >= 90:
                            raise MissingIngressRoute(f"{ident}: public ingress still presents the default certificate")
                    else:
                        missing_since = None
            backoff(deadline, 7)
        else:
            raise RuntimeError(f"{ident} exceeded startup deadline; see {state}/startup.jsonl")
        expected = subprocess.check_output(["elan", "run", (root/"lean-toolchain").read_text().strip(), "lean", "--githash"]).strip()
        if not re.fullmatch(rb"[0-9a-f]{40}", expected) or test.stdout.strip() != expected:
            raise RuntimeError("pod and local verifier compiler revisions differ")
        # The remote operands are encoded argv. No SSH shell sees model text.
        with tempfile.TemporaryFile() as archive:
            subprocess.run(["tar", "-C", str(root), "--exclude=.lake", "--exclude=.beam", "-cf", "-", "."],
                           stdout=archive, check=True)
            archive.seek(0)
            subprocess.run(ssh(state, ["stage"]), stdin=archive, check=True, timeout=120)
        manifest = subprocess.run(ssh(state, ["exec","cat","/project/lake-manifest.json"]),
                                  capture_output=True, check=True, timeout=20)
        if json.loads(manifest.stdout).get("packages") != json.loads((root/"lake-manifest.json").read_text()).get("packages"):
            raise RuntimeError("prepared pod dependencies differ from trusted manifest")
        runtime = subprocess.run(ssh(state, ["exec", "/opt/lean/bin/lean-beam", "--version"]),
                                 capture_output=True, check=True, timeout=25)
        architecture = subprocess.run(ssh(state, ["exec", "uname", "-m"]),
                                      capture_output=True, check=True, timeout=25)
        (state / "runtime.json").write_text(json.dumps({
            "pod_id": ident, "status": status, "image": image, "ready_seconds": round(time.monotonic()-started, 2),
            "compiler": expected.decode(), "beam": runtime.stdout.decode().strip(),
            "architecture": architecture.stdout.decode().strip()
        }, indent=2) + "\n")
        print(ident, flush=True)
    except BaseException:
        try:
            delete(state)
        except Exception as cleanup:
            print(f"Tapis cleanup pending for {state}: {cleanup}", file=sys.stderr)
        raise


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("action", choices=["create", "exec", "read", "write", "delete"])
    p.add_argument("state", type=Path)
    p.add_argument("arguments", nargs=argparse.REMAINDER)
    a = p.parse_args()
    state = a.state.resolve()
    if a.action == "create":
        root, image, memory, cpus = a.arguments
        with startup_slot():
            create(state, Path(root), image, int(memory), int(cpus))
    elif a.action == "delete":
        delete(state)
    else:
        args = ssh(state, [a.action, *a.arguments])
        # No provider credentials or SSH agent in the transport child environment.
        os.execvpe(args[0], args, {"PATH": "/usr/bin:/bin", "HOME": str(Path.home()), "LANG": "C.UTF-8"})


if __name__ == "__main__":
    os.umask(0o077)
    main()
