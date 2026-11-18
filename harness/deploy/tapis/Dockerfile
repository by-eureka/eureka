# syntax=docker/dockerfile:1
# "sdk" is the pinned Eureka runtime supplied by Bake.
FROM ubuntu:24.04
COPY --from=sdk /opt/lean /opt/lean
COPY --from=sdk /opt/elan /opt/elan
# Cache archives contain mode-0600 oleans. Make them readable to the
# runtime user while keeping every dependency owned by root.
COPY --from=sdk --chown=0:0 --chmod=u=rwX,go=rX /project /project
ENV PATH=/opt/lean/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ENV ELAN_HOME=/opt/elan
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server libseccomp2 python3 git procps ripgrep curl ca-certificates libc6-dev libgmp10 libuv1t64 \
    && rm -f /etc/ssh/ssh_host_* && rm -rf /var/lib/apt/lists/* \
    && useradd -M -u 11001 -d /home/prover -s /bin/sh -p '*' prover \
    && install -d -m 0755 /run/sshd /home/prover \
    && install -d -o prover -g prover -m 0700 /work /run/eureka \
    && for repo in /project/.lake/packages/*; do git config --system --add safe.directory "$repo"; done
COPY --chmod=0555 pod.py /usr/local/bin/eureka-pod
USER 11001:11001
ENV HOME=/home/prover
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/eureka-pod"]
CMD ["serve"]
