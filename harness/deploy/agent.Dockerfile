# syntax=docker/dockerfile:1
# Native Lean/Beam SDK plus the template's pinned Mathlib dependencies.
FROM ubuntu:24.04 AS build
ARG TARGETARCH
ARG LEAN_VERSION=4.35.0
ARG LEAN_SHA256_AMD64=7289597a44543246c9227b8cae501767e095ce17f6d448b95fc4a8dc97268f6d
ARG LEAN_SHA256_ARM64=c8f0f8a3920e839718b1343e6781fd80a3e9b1024fbcd7078eb30f54e806b028
ARG BEAM_REV
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git zstd cmake make gcc libc6-dev libgmp-dev python3 \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
      amd64) archive=linux; checksum="$LEAN_SHA256_AMD64" ;; \
      arm64) archive=linux_aarch64; checksum="$LEAN_SHA256_ARM64" ;; \
      *) exit 1 ;; \
    esac && \
    curl --fail --location --retry 3 \
      "https://github.com/leanprover/lean4/releases/download/v${LEAN_VERSION}/lean-${LEAN_VERSION}-${archive}.tar.zst" \
      -o /tmp/lean.tar.zst && \
    echo "$checksum  /tmp/lean.tar.zst" | sha256sum --check --strict && \
    mkdir /opt/lean && tar --zstd -xf /tmp/lean.tar.zst -C /opt/lean --strip-components=1 && \
    rm /tmp/lean.tar.zst
ENV PATH=/opt/lean/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN test -n "$BEAM_REV" && git init /opt/beam-source && cd /opt/beam-source && \
    git remote add origin https://github.com/leanprover/lean-beam.git && \
    git fetch --depth=1 origin "$BEAM_REV" && git checkout --detach FETCH_HEAD && \
    test "$(git rev-parse HEAD)" = "$BEAM_REV" && \
    cmake -DLEAN_PREFIX=/opt/lean -P scripts/build-toolchain.cmake && \
    cmake -DLEAN_PREFIX=/opt/lean -DPREFIX=/opt/lean -P scripts/install-toolchain.cmake

FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git python3 curl libc6-dev libgmp10 libuv1t64 ripgrep \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /opt/lean /opt/lean
ENV PATH=/opt/lean/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
COPY --chown=1000:1000 examples/simple /project
ENV ELAN_HOME=/opt/elan
RUN mkdir -p /opt/elan/toolchains && \
    ln -s /opt/lean "/opt/elan/toolchains/$(sed -e 's|/|--|g' -e 's|:|---|g' /project/lean-toolchain)"
WORKDIR /project
ENV HOME=/tmp
USER 1000:1000
RUN lake exe cache get && lake build Proofs
# setup-file traverses server artifacts and hashes that a normal batch build may
# not touch. Materialize them before dependencies become read-only in attempts.
RUN printf 'import Mathlib\n' > RuntimeImports.lean && \
    lake setup-file RuntimeImports.lean > /tmp/runtime-setup.json && \
    rm RuntimeImports.lean /tmp/runtime-setup.json
USER root
RUN apt-get update && apt-get install -y --no-install-recommends procps && rm -rf /var/lib/apt/lists/*
USER 1000:1000
CMD ["sleep", "infinity"]
