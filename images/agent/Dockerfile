# syntax=docker/dockerfile:1
#
# The ingest cycle image: the `ctx` binary, the ingest runner and the two skills of one ctx-memory
# release, and the Oh My Pi (omp) release that runner/omp.lock pins. Its entrypoint is the
# release's runner/run-cycle.sh, which runs one cycle: a headless omp session that turns the
# pending windows into records through the ctx server. The image meets the contract of
# docs/deployment.md, "The ingest cycle".
#
# It carries no model provider. The deployment mounts its own provider file and names it, and the
# model, in CTX_MODELS_FILE and CTX_MODEL; run-cycle.sh refuses to start a session without them
# (runner/models.example.yml shows the file's shape).
#
# The entrypoint runs as root (no USER line) because it has to: it checks what the session can
# touch, gives the session its transcript directory on the share and takes it back afterwards. The
# session itself runs as ctx-agent, which can read the share but write nothing on it except that
# directory, because the session reads text that people outside the deployment wrote.
#
# .github/workflows/images.yml builds it for each release and publishes it as
# ghcr.io/sjawhar/ctx-memory/agent. By hand, from the repository root, with the values of that
# release's SHA256SUMS:
#   docker build --build-arg CTX_VERSION=<version> --build-arg CTX_SHA256=<sha256> \
#     --build-arg CTX_SHARE_SHA256=<sha256> images/agent

# The release this image installs, and the SHA-256 of each archive it takes from it as the
# release's SHA256SUMS lists them: the x86_64 `ctx` archive and the share archive, whose skills/
# and runner/ trees the cycle runs. A release asset can be replaced, so each archive is checked
# against its digest before anything in it is unpacked. None has a default: an image installs the
# release its build names, and no other.
ARG CTX_VERSION
ARG CTX_SHA256
ARG CTX_SHARE_SHA256
ARG CTX_RELEASE_URL=https://github.com/sjawhar/ctx-memory/releases/download

# debian:trixie-slim, pinned by the digest of its multi-platform index (`crane digest
# debian:trixie-slim`, 2026-11-02), because a tag can be moved to another image.
FROM debian@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4 AS release
ARG CTX_VERSION
ARG CTX_SHA256
ARG CTX_SHARE_SHA256
ARG CTX_RELEASE_URL
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# /out holds what the final stage copies, laid out as it lands: bin/ goes to /usr/local/bin and
# opt/ to /opt/ctx-memory. Each archive's digest is checked before anything in it is extracted; the
# `ctx` archive holds that one file. Of the share archive only the files the image contract names
# are installed: the two skills, run-cycle.sh, policy.yml and ctx-record.ts. runner/omp.lock is
# read below and not installed, and runner/models.example.yml is not installed, because the
# provider file is the deployment's own.
RUN set -eu; \
    case "$CTX_VERSION" in "" | *[!0-9A-Za-z.-]*) echo "CTX_VERSION must be a release version such as 0.2.9: '$CTX_VERSION'" >&2; exit 1 ;; esac; \
    for sum in "$CTX_SHA256" "$CTX_SHARE_SHA256"; do \
        case "$sum" in *[!0-9a-f]*) echo "CTX_SHA256 and CTX_SHARE_SHA256 must be lowercase hex: '$sum'" >&2; exit 1 ;; esac; \
        test "${#sum}" -eq 64 || { echo "CTX_SHA256 and CTX_SHARE_SHA256 must be the archives' SHA-256 from the release's SHA256SUMS" >&2; exit 1; }; \
    done; \
    url="${CTX_RELEASE_URL}/v${CTX_VERSION}"; \
    curl -fsSL --retry 3 -o /tmp/ctx.tar.gz "$url/ctx-x86_64-unknown-linux-musl.tar.gz"; \
    curl -fsSL --retry 3 -o /tmp/share.tar.gz "$url/ctx-memory-${CTX_VERSION}-share.tar.gz"; \
    printf '%s  %s\n' "$CTX_SHA256" /tmp/ctx.tar.gz "$CTX_SHARE_SHA256" /tmp/share.tar.gz | sha256sum --check --strict; \
    test "$(tar -tzf /tmp/ctx.tar.gz)" = ctx; \
    mkdir -p /out/bin /out/opt /tmp/share; \
    tar -xzf /tmp/ctx.tar.gz -C /out/bin --no-same-owner ctx; \
    tar -xzf /tmp/share.tar.gz -C /tmp/share --no-same-owner; \
    rm /tmp/ctx.tar.gz /tmp/share.tar.gz; \
    chmod 0755 /out/bin/ctx; \
    install -m 0755 /tmp/share/runner/run-cycle.sh /out/bin/run-cycle.sh; \
    install -m 0644 /tmp/share/runner/policy.yml /tmp/share/runner/ctx-record.ts /out/opt/; \
    cp -R /tmp/share/skills /out/opt/skills; \
    find /out/opt/skills -type d -exec chmod 0755 {} +; \
    find /out/opt/skills -type f -exec chmod 0644 {} +; \
    test "$(/out/bin/ctx --version)" = "ctx $CTX_VERSION"
# omp is the release runner/omp.lock names, the build the runner's own tests drove, read from the
# lock in the share archive just unpacked, so the image and the runner cannot name two omp builds.
# The archive holds one file, <dir>/bin/omp, so two components are stripped. Its digest is checked
# before anything is extracted, because a release asset can be replaced.
RUN set -eu; \
    lock=/tmp/share/runner/omp.lock; \
    release="$(sed -n 's/^OMP_RELEASE=//p' "$lock")"; \
    sha256="$(sed -n 's/^OMP_SHA256_LINUX_X64=//p' "$lock")"; \
    case "$release" in "" | *[!A-Za-z0-9._-]*) echo "omp.lock names no usable OMP_RELEASE: '$release'" >&2; exit 1 ;; esac; \
    case "$sha256" in *[!0-9a-f]*) echo "omp.lock's OMP_SHA256_LINUX_X64 is not lowercase hex: '$sha256'" >&2; exit 1 ;; esac; \
    test "${#sha256}" -eq 64; \
    curl -fsSL --retry 3 -o /tmp/omp.tar.gz \
        "https://github.com/sjawhar/oh-my-pi/releases/download/${release}/omp-${release}-linux-x64.tar.gz"; \
    printf '%s  %s\n' "$sha256" /tmp/omp.tar.gz | sha256sum --check --strict; \
    tar -xzf /tmp/omp.tar.gz -C /out/bin --strip-components=2 --no-same-owner --wildcards '*/bin/omp'; \
    rm -r /tmp/omp.tar.gz /tmp/share; \
    test -x /out/bin/omp

# debian:trixie-slim
FROM debian@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG CTX_VERSION
LABEL org.opencontainers.image.source=https://github.com/sjawhar/ctx-memory \
      org.opencontainers.image.title=ctx-memory-agent \
      org.opencontainers.image.description="The ctx-memory ingest cycle: runner/run-cycle.sh with ctx and Oh My Pi" \
      org.opencontainers.image.licenses=MIT \
      org.opencontainers.image.version=${CTX_VERSION}
# ca-certificates: the roots ctx checks the ctx server against, and omp the model API. jq:
# run-cycle.sh reads `ctx pending --json` with it. setpriv, which drops the session to ctx-agent,
# is util-linux's, and timeout and tail are coreutils'; all three are in the base image already.
# ctx-agent has a fixed uid and gid, 10001, so a deployment can grant it read access to the share.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates jq \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 ctx-agent \
    && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /home/ctx-agent \
        --shell /usr/sbin/nologin ctx-agent
# ctx, omp and run-cycle.sh to /usr/local/bin, and the policy, the record tools and the skills to
# /opt/ctx-memory, all root's and closed to writes. omp loads ctx-record.ts, the session's only
# writes, with --trusted-extension. That flag stops extension discovery and nothing more: omp still
# discovers custom tools, plugin tools and MCP servers under HOME and its working directory when
# it starts. Those are closed below, by making every place they are read from root's.
COPY --from=release /out/bin/ /usr/local/bin/
COPY --from=release /out/opt/ /opt/ctx-memory/
# HOME is where omp reads its settings (<HOME>/.omp/agent/config.yml), its model provider
# (models.yml, which run-cycle.sh writes there from CTX_MODELS_FILE) and the configuration it
# discovers, and where ctx would read a user config (~/.config/ctx). It is ctx-agent's home, the
# session's user, and all root's and closed to writes, so the session can add nothing omp or ctx
# would read there. WORKDIR is an empty directory and not HOME, because omp started in the home
# directory moves itself to a temporary one; it is root's for the same reason.
ENV HOME=/home/ctx-agent
WORKDIR /workspace
# What omp writes at run time (agent.db, logs, run-time sockets, the native module it extracts
# from its own binary) goes to the XDG directories instead of HOME, while its configuration and
# every place it discovers tools from stay in HOME. The cycle's sessions go to the share
# (run-cycle.sh --session-dir).
ENV XDG_DATA_HOME=/var/lib/omp/data XDG_STATE_HOME=/var/lib/omp/state XDG_CACHE_HOME=/var/lib/omp/cache
# The release's two skills, and no others: a cycle's omp session reads
# skill://ctx-memory-ingest for the procedure and skill://ctx-memory for quotes and tiers.
COPY <<'EOF' /home/ctx-agent/.omp/agent/config.yml
skills:
  customDirectories:
    - /opt/ctx-memory/skills
EOF
# HOME and the working directory are root's and 0755, every file in them root's and 0644, with
# a .bash_profile of root's beside the shell's startup files from /etc/skel (omp sources
# ~/.bashrc when it builds a shell snapshot, and a login shell reads the rest), so none can be
# added or replaced. The XDG directories are the session's to add files to, and root's with the
# sticky bit set, so the session cannot replace what root keeps there: the plugin directory,
# whose node_modules omp loads tools from, empty. /etc/ctx-memory is a directory of root's for
# the deployment's provider file. run-cycle.sh checks all of it before each session.
RUN set -eu; \
    touch /home/ctx-agent/.bash_profile; \
    install -d -o root -g root -m 0755 /etc/ctx-memory; \
    chown -R root:root /home/ctx-agent /workspace /opt/ctx-memory; \
    find /home/ctx-agent /workspace /opt/ctx-memory -type d -exec chmod 0755 {} +; \
    find /home/ctx-agent /opt/ctx-memory -type f -exec chmod 0644 {} +; \
    for dir in data state cache; do install -d -o root -g ctx-agent -m 1775 "/var/lib/omp/$dir/omp"; done; \
    install -d -o root -g root -m 0755 /var/lib/omp/data/omp/plugins
# omp's native module is embedded in the binary and extracted on its first run, which is this
# step, so a cycle starts without writing it. omp extracts one of two variants, chosen by the CPU:
# `modern` where it has AVX2, `baseline` elsewhere. The build host has AVX2, so its first run
# extracts `modern`, and a second run with PI_NATIVE_VARIANT=baseline extracts the other, so the
# image runs on either kind of CPU once the directory is closed to the session below. The same
# run checks that the skills are the session's own: `omp skill list` resolves skills exactly as a
# session in this directory does, so a config that does not reach them fails the build here. It
# runs as the session's user, with HOME already closed to it, so a build whose omp needs to write
# there fails here too.
USER ctx-agent
RUN set -eu; \
    ctx --version; \
    omp --version; \
    PI_NATIVE_VARIANT=baseline omp skill list --json > /dev/null; \
    omp skill list --json > /tmp/skills.json; \
    jq -e '([.skills[] | select(.source == "custom:user") | .name] | sort) == ["ctx-memory", "ctx-memory-ingest"]' /tmp/skills.json > /dev/null; \
    rm /tmp/skills.json; \
    for variant in modern baseline; do \
        set -- "$XDG_DATA_HOME"/omp/natives/*/"pi_natives.linux-x64-$variant.node"; \
        test -f "$1" || { echo "omp extracted no $variant native module" >&2; exit 1; }; \
    done; \
    rm -rf "$XDG_STATE_HOME/omp/logs"
# The local state directory ctx would otherwise resolve from HOME. The cycle reads and writes
# through the server, so nothing of consequence lands here, but the directory is the container's
# own and never the share. A container that names its own CTX_STATE_DIR overrides it.
USER root
ENV CTX_STATE_DIR=/var/lib/ctx
RUN install -d -o ctx-agent -g ctx-agent /var/lib/ctx
# The native module omp loads into its own process is root's from here on, so the session can
# neither rewrite it nor, in the sticky data directory, put another in its place.
RUN chown -R root:root /var/lib/omp/data/omp/natives && chmod -R go-w /var/lib/omp/data/omp/natives
# `read issue://…` and `read pr://…` run `gh` at the read tier, which the policy approves, and
# omp has no setting that turns them off: the image carries no gh, and a build that gains one
# fails here.
RUN ! command -v gh
# No arguments run one cycle; `maintain <pass>` runs one maintenance pass (docs/deployment.md).
ENTRYPOINT ["run-cycle.sh"]
