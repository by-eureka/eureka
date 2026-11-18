#!/usr/bin/env python3
"""TACC Pods lifecycle for the Eureka controller. The model never sees these credentials."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

API = "https://tacc.tapis.io/v3/pods"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        return None


def api(method, path, payload=None):
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
        with urllib.request.build_opener(NoRedirect()).open(req, timeout=45) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404 and method == "GET":
            return None
        try:
            detail = str(json.load(error).get("message", "")).replace(token, "[redacted]")
            detail = re.sub(r"eyJ[\w.-]{20,}", "[redacted]", detail)[:450]
        except (ValueError, AttributeError):
            detail = ""
        raise RuntimeError(f"Tapis {method} {path}: HTTP {error.code}: {detail}") from None
    if result.get("status") != "success":
        raise RuntimeError(f"Tapis {method} {path} failed")
    return result["result"]


def ssh(state, args):
    meta = json.loads((state / "pod.json").read_text())
    host = meta["id"] + ".pods.tacc.tapis.io"
    encoded = base64.b64encode(json.dumps(args).encode()).decode()
    cmd = ["ssh", "-F", "/dev/null", "-T", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
           "-o", "IdentityAgent=none", "-o", "ClearAllForwardings=yes", "-o", "StrictHostKeyChecking=yes",
           "-o", "GlobalKnownHostsFile=/dev/null", "-o", "UserKnownHostsFile=" + str(state / "known_hosts"),
           "-o", "ConnectTimeout=12", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3",
           "-o", "ProxyCommand=openssl s_client -quiet -verify_return_error -verify_hostname %h -servername %h -connect %h:443 2>/dev/null",
           "-i", str(state / "identity"), "prover@" + host, encoded]
    return cmd


def delete(state):
    meta = json.loads((state / "pod.json").read_text())
    path = "/" + meta["id"]
    row = api("GET", path)
    if row is not None:
        if row.get("description") != meta["description"]:
            raise RuntimeError("refusing to delete pod owned by another attempt")
        api("DELETE", path)
    if api("GET", path) is not None:
        raise RuntimeError("Tapis still reports the pod after deletion")
    (state / "identity").unlink(missing_ok=True)
    (state / "deleted").touch()


def create(state, root, image, memory, cpus):
    if not re.fullmatch(r"debganguly/infinilean:[A-Za-z0-9_.-]+", image) or image.endswith(":latest"):
        raise ValueError("expected a versioned debganguly/infinilean image tag")
    state.mkdir(mode=0o700)
    ident = "eur" + secrets.token_hex(8)
    description = "Eureka attempt " + secrets.token_hex(16)
    (state / "pod.json").write_text(json.dumps({"id": ident, "description": description, "image": image}) + "\n")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-C", "", "-N", "", "-f", str(state/"identity")], check=True)
    key = " ".join((state / "identity.pub").read_text().split()[:2])
    template = {"pod_id": ident, "description": description, "image": image,
                "volume_mounts": {"/etc/ssh/eureka_authorized_keys": {
                    "type": "ephemeral", "config_content": "restrict " + key + "\n",
                    "config_permissions": "0444"}},
                "resources": {"cpu_request": cpus, "cpu_limit": cpus, "mem_request": memory, "mem_limit": memory},
                "networking": {"default": {"protocol": "tcp", "port": 2222}},
                "time_to_stop_default": int(os.getenv("TAPIS_TIME_TO_STOP", "3600"))}
    path = "/" + ident
    started = time.monotonic()
    last_status = None
    try:
        api("POST", "", template)
        deadline = time.monotonic() + int(os.getenv("TAPIS_START_TIMEOUT", "900"))
        while time.monotonic() < deadline:
            row = api("GET", path)
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
            if status == "AVAILABLE":
                # Bootstrap the host key through the authenticated Tapis control plane.
                result = api("POST", path + "/exec", {"commands": [["cat", "/run/eureka/host.pub"]],
                                                      "fail_on_non_success": False})
                output = result.get("execution_results", []) if isinstance(result, dict) else result
                if output and isinstance(output[0], dict):
                    public = output[0].get("stdout", "").strip().split()
                    if len(public) >= 2 and public[0] == "ssh-ed25519" and re.fullmatch(r"[A-Za-z0-9+/]+=*", public[1]):
                        (state / "known_hosts").write_text(f"{ident}.pods.tacc.tapis.io {public[0]} {public[1]}\n")
                        try:
                            test = subprocess.run(ssh(state, ["exec", "/opt/lean/bin/lean", "--githash"]),
                                                  capture_output=True, timeout=25)
                        except subprocess.TimeoutExpired:
                            test = None
                        if test is not None and test.returncode == 0:
                            break
            time.sleep(3)
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
        delete(state)
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
