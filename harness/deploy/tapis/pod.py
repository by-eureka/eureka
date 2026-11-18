#!/usr/bin/python3
"""Unprivileged SSH transport for one private Eureka workspace. No tenant or model secrets."""
import base64
import ctypes
import errno
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile

STATE = Path("/run/eureka")
WORK = Path("/work")
MAX_FILE = 4 * 1024 * 1024


def serve():
    auth = "/etc/ssh/eureka_authorized_keys"
    if not os.path.isfile(auth) or os.access(auth, os.W_OK):
        raise RuntimeError("mount the controller public key read-only at " + auth)
    os.umask(0o077)
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(STATE/"host")], check=True)
    options = ["Port=2222", "HostKey=/run/eureka/host", "PidFile=/run/eureka/sshd.pid",
               "AuthorizedKeysFile=" + auth, "PasswordAuthentication=no", "KbdInteractiveAuthentication=no",
               "AuthenticationMethods=publickey", "PermitRootLogin=no", "AllowUsers=prover", "UsePAM=no",
               "PermitUserEnvironment=no", "PermitUserRC=no", "PermitTTY=no", "DisableForwarding=yes",
               "ForceCommand=/usr/local/bin/eureka-pod command"]
    args = ["/usr/sbin/sshd", "-D", "-e", "-f", "/dev/null"]
    for option in options:
        args.extend(["-o", option])
    os.execv(args[0], args)


def restrict_network():
    # The SSH server owns the network connection. Tool processes inherit only
    # the session pipes. Apply the same filter to Lean, shell, and file tools.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
        raise OSError(ctypes.get_errno(), "prctl")
    sc = ctypes.CDLL("libseccomp.so.2")
    sc.seccomp_init.argtypes = [ctypes.c_uint32]
    sc.seccomp_init.restype = ctypes.c_void_p
    sc.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    sc.seccomp_rule_add_array.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint, ctypes.c_void_p]
    sc.seccomp_load.argtypes = [ctypes.c_void_p]
    sc.seccomp_release.argtypes = [ctypes.c_void_p]
    ctx = sc.seccomp_init(0x7FFF0000)  # SCMP_ACT_ALLOW
    if not ctx:
        raise RuntimeError("seccomp_init failed")
    class Comparison(ctypes.Structure):
        _fields_ = [("arg", ctypes.c_uint), ("op", ctypes.c_int), ("a", ctypes.c_uint64), ("b", ctypes.c_uint64)]
    try:
        # Permit Unix-domain socket creation only. Block alternative async
        # socket creation and cross-process access to the SSH supervisor.
        for name in [b"socket", b"socketpair", b"io_uring_setup", b"ptrace", b"process_vm_writev", b"pidfd_getfd"]:
            syscall = sc.seccomp_syscall_resolve_name(name)
            if syscall < 0:
                raise RuntimeError("unrecognized syscall " + name.decode())
            cmp = Comparison(0, 1, socket.AF_UNIX, 0) if name in (b"socket", b"socketpair") else None  # NE
            if sc.seccomp_rule_add_array(ctx, 0x50000 | errno.EPERM, syscall, int(cmp is not None), ctypes.byref(cmp) if cmp else None):
                raise RuntimeError("failed to add seccomp rule")
        if sc.seccomp_load(ctx):
            raise RuntimeError("failed to apply seccomp rules")
    finally:
        sc.seccomp_release(ctx)


def file_op(op, path):
    from contextlib import ExitStack
    parts = path.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise ValueError("invalid workspace path")
    with ExitStack() as stack:
        fd = os.open(WORK, os.O_DIRECTORY)
        stack.callback(os.close, fd)
        for part in parts[:-1]:
            fd = os.open(part, os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            stack.callback(os.close, fd)
        name = parts[-1]
        if op == "read":
            with os.fdopen(os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd), "rb") as f:
                if not stat.S_ISREG(os.fstat(f.fileno()).st_mode):
                    raise ValueError("expected regular file")
                before = os.fstat(f.fileno())
                data = f.read(MAX_FILE + 1)
                after = os.fstat(f.fileno())
                if (before.st_size, before.st_ctime_ns) != (after.st_size, after.st_ctime_ns):
                    raise RuntimeError("workspace file changed during capture")
            if len(data) > MAX_FILE:
                raise ValueError("file too large")
            sys.stdout.buffer.write(data)
        else:
            data = sys.stdin.buffer.read(MAX_FILE + 1)
            if len(data) > MAX_FILE:
                raise ValueError("file too large")
            with tempfile.NamedTemporaryFile(dir=f"/proc/self/fd/{fd}", delete=False) as f:
                f.write(data)
            tmp = os.path.basename(f.name)
            try:
                os.replace(tmp, name, src_dir_fd=fd, dst_dir_fd=fd)
            finally:
                try:
                    os.unlink(tmp, dir_fd=fd)
                except FileNotFoundError:
                    pass


def command():
    args = json.loads(base64.b64decode(os.environ["SSH_ORIGINAL_COMMAND"], validate=True))
    if not isinstance(args, list) or not args or not all(isinstance(s, str) for s in args):
        raise ValueError("expected a command argument list")
    os.chdir(WORK)
    restrict_network()
    os.environ.clear()
    os.environ.update(HOME="/tmp", PATH="/opt/lean/bin:/usr/local/bin:/usr/bin:/bin",
                      ELAN_HOME="/opt/elan", BEAM_SESSION_ROOT="/work/.beam", LANG="C.UTF-8")
    if args[0] in ("read", "write") and len(args) == 2:
        file_op(*args)
    elif args == ["stage"]:
        # The controller supplies a tar of its own quiescent template exactly once.
        subprocess.run(["tar", "-xf", "-", "--no-same-owner", "--no-same-permissions"], check=True)
        (WORK / ".lake").mkdir(exist_ok=True)
        (WORK / ".lake/packages").symlink_to("/project/.lake/packages", target_is_directory=True)
    elif args[0] == "exec" and len(args) >= 2:
        os.execvp(args[1], args[1:])
    else:
        raise ValueError("unknown operation")


if __name__ == "__main__":
    if sys.argv[1:] == ["serve"]:
        serve()
    elif sys.argv[1:] == ["command"]:
        command()
    else:
        sys.exit(2)
