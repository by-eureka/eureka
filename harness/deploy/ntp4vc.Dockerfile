# Build the support-only context produced by scripts/prepare-ntp4vc.py.
#   docker build -f harness/deploy/ntp4vc.Dockerfile -t eureka-ntp4vc:dev runs/BATCH/image
FROM eureka-agent:dev
COPY --chown=1000:1000 ntpSupport /project/.lake/packages/ntpSupport
COPY --chown=1000:1000 project /project
COPY --chown=1000:1000 modules.json /tmp/ntp-modules.json
WORKDIR /project
RUN MATHLIB_NO_CACHE_ON_UPDATE=1 lake update ntpSupport
RUN LEAN_NUM_THREADS=4 python3 - <<'PY'
import json
import subprocess
subprocess.run(["lake", "build", *json.load(open('/tmp/ntp-modules.json'))], check=True)
PY
WORKDIR /project
