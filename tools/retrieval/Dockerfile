# syntax=docker/dockerfile:1
ARG BEAM_IMAGE=eureka-lean:local
FROM ${BEAM_IMAGE} AS python
USER root
RUN apt-get update && apt-get install -y --no-install-recommends python3-venv ripgrep \
    && rm -rf /var/lib/apt/lists/*
COPY requirements.txt /opt/retrieval/requirements.txt
RUN python3 -m venv /opt/retrieval/venv && \
    /opt/retrieval/venv/bin/pip install --no-cache-dir -r /opt/retrieval/requirements.txt
USER 1000:1000

FROM python AS loogle-build
USER root
ARG LOOGLE_REV=9f11169aaebf1ed1e7dcc4077f2aafe0fcf66fd0
RUN git init /opt/loogle && cd /opt/loogle && \
    git remote add origin https://github.com/nomeata/loogle.git && \
    git fetch --depth=1 origin ${LOOGLE_REV} && git checkout --detach FETCH_HEAD && \
    cp /project/lean-toolchain lean-toolchain && lake build loogle
RUN chown 1000:1000 /opt/retrieval
USER 1000:1000
WORKDIR /project
RUN lake env /opt/loogle/.lake/build/bin/loogle --index-mode write \
    --index-file /opt/retrieval/mathlib.idx --max-results 1 --json '"Nat.add_comm"'

FROM python AS client
COPY *.py LICENSE NOTICE /opt/retrieval/

FROM client AS service
COPY --from=loogle-build /opt/loogle/.lake/build /opt/loogle/.lake/build
COPY --from=loogle-build /opt/loogle/LICENSE /opt/loogle/LICENSE
COPY --from=loogle-build /opt/retrieval/mathlib.idx /opt/retrieval/mathlib.idx
WORKDIR /project
ENTRYPOINT ["/opt/retrieval/venv/bin/python", "/opt/retrieval/service.py"]
