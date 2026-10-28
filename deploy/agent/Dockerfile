# syntax=docker/dockerfile:1
#
# The agent cycle image: the `ctx` binary, Oh My Pi (omp) and this repository's two skills. It is
# the image of the Cloud Run job `ctx-agent`, which runs deploy/agent/run-cycle.sh once an hour:
# one headless omp session that turns the pending windows into records through the ctx server.
# The entrypoint runs as root (no USER line) because it has to: the Filestore share the job
# mounts lets only uid 0 write, so only root can hand the session a directory of its own for the
# JSONL that measures each cycle's model cost, and give it back to root afterwards. The session
# itself is handed to ctx-agent, a user that can read the share but write nothing on it except
# that directory, because the session reads text that people outside the organization wrote
# (deploy/agent/run-cycle.sh, which refuses to start a session when that does not hold).
#
# Build from the repository root:
#   docker build -f deploy/agent/Dockerfile -t ctx-agent:dev .

# Each base image is pinned by the digest of its multi-platform index, read with `crane digest`
# on 2026-10-28, because a tag can be moved to another image; the tag the digest was read from
# is in the comment above its FROM. RUST_VERSION names the toolchain the pinned rust image ships,
# so it changes with that digest.
ARG RUST_VERSION=1.99.1
# The omp release Legion pins in its .omp-pin (github:sjawhar/oh-my-pi@18.12.2-sami.20261026-135627),
# so this image's agent is the build every Legion worker runs. OMP_SHA256 is the asset's digest as
# the GitHub release reports it (gh release view --json assets).
ARG OMP_RELEASE=v18.12.2-sami.20261026-135627
ARG OMP_SHA256=b513556d000f4b98be33dfb42b0fa88879455c388bb6c8fdcbeb5e032f73fdfa

# rust:1.99.1-slim-trixie
FROM rust@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build
ARG RUST_VERSION
# rust-toolchain.toml names the `stable` channel, which rustup would resolve by downloading a
# second toolchain over the network, ignoring the one this image ships and the version pinned
# above. RUSTUP_TOOLCHAIN outranks a toolchain file, so the build uses the image's own.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
# A small crate on a 4-vCPU runner (GitHub's standard runner) and on a shared developer box: an
# unbounded job count buys nothing here and starves everything else sharing the machine.
ENV CARGO_BUILD_JOBS=4
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked --bin ctx \
    && install -D -m 0755 target/release/ctx /out/ctx

# debian:trixie-slim
FROM debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad AS omp
ARG OMP_RELEASE
ARG OMP_SHA256
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# The archive holds one file, <release>/bin/omp, so two components are stripped. The digest is
# checked before anything is extracted: a release asset is mutable, and this build runs the binary
# it unpacks in the step that extracts omp's native module.
RUN set -eu; \
    url="https://github.com/sjawhar/oh-my-pi/releases/download/${OMP_RELEASE}/omp-${OMP_RELEASE}-linux-x64.tar.gz"; \
    curl -fsSLo /tmp/omp.tar.gz "$url"; \
    printf '%s  %s\n' "$OMP_SHA256" /tmp/omp.tar.gz | sha256sum --check --strict; \
    mkdir -p /out; \
    tar -xzf /tmp/omp.tar.gz -C /out --strip-components=2 --no-same-owner; \
    rm /tmp/omp.tar.gz; \
    test -x /out/omp

# debian:trixie-slim
FROM debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
LABEL org.opencontainers.image.source=https://github.com/sjawhar/ctx-memory
# ca-certificates: the roots omp checks the Anthropic API against, and ctx the ctx server's own
# Cloud Run certificate. jq: run-cycle.sh reads `ctx pending --json` with it. setpriv, which drops
# the session to ctx-agent, is util-linux's and is already in the base image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates jq \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/ctx-agent --shell /usr/sbin/nologin ctx-agent
COPY --from=build /out/ctx /usr/local/bin/ctx
COPY --from=omp /out/omp /usr/local/bin/omp
COPY skills /opt/ctx-memory/skills
COPY deploy/agent/run-cycle.sh /usr/local/bin/run-cycle.sh
COPY deploy/agent/policy.yml /opt/ctx-memory/policy.yml
# The two record tools, the session's only writes. root's, like the policy: omp loads this one
# module with --trusted-extension. That flag stops extension discovery and nothing more: omp
# still discovers custom tools (.omp/tools in its working directory, ~/.omp/agent/tools), plugin
# tools and MCP servers (.mcp.json, ~/.omp/agent/mcp.json, other agents' files under HOME) when
# it starts. Those are closed below, by making every place they are read from root's.
COPY deploy/agent/ctx-record.ts /opt/ctx-memory/ctx-record.ts
# HOME is where omp reads its settings (<HOME>/.omp/agent/config.yml), its model credential
# (models.yml) and the configuration it discovers, and where ctx would read a user config
# (~/.config/ctx). It is ctx-agent's home, because that is the user the session runs as, and it
# is all root's and closed to writes, so the session can add nothing omp or ctx would read there.
# WORKDIR is an empty directory and not HOME, because omp started in the home directory moves
# itself to a temporary one unless --allow-home is given; it is root's for the same reason.
ENV HOME=/home/ctx-agent
WORKDIR /workspace
# What omp writes at run time (agent.db, logs, run-time sockets, the native module it extracts
# from its own binary) goes to the XDG directories instead of HOME: omp uses $XDG_DATA_HOME/omp,
# $XDG_STATE_HOME/omp and $XDG_CACHE_HOME/omp whenever they exist
# (packages/utils/src/dirs.ts:363-419 at the pinned release), and keeps its configuration and
# every place it discovers tools from in HOME. The cycle's sessions go to the share
# (run-cycle.sh --session-dir).
ENV XDG_DATA_HOME=/var/lib/omp/data XDG_STATE_HOME=/var/lib/omp/state XDG_CACHE_HOME=/var/lib/omp/cache
# The two skills of this repository, and no others: a cycle's omp session reads
# skill://ctx-memory-ingest for the procedure and skill://ctx-memory for quotes and tiers.
COPY <<'EOF' /home/ctx-agent/.omp/agent/config.yml
skills:
  customDirectories:
    - /opt/ctx-memory/skills
EOF
# The model key is never built into the image: models.yml names the file run-cycle.sh writes it
# to from Secret Manager, so the session resolves the credential without holding it in its
# environment.
COPY deploy/agent/models.yml /home/ctx-agent/.omp/agent/models.yml
# HOME and the working directory are root's and 0755, every file in them root's and 0644, with
# a .bash_profile of root's beside the shell's startup files from /etc/skel (omp sources
# ~/.bashrc when it builds a shell snapshot, and a login shell reads the rest), so none can be
# added or replaced. The XDG directories are the session's to add files to, and root's with the
# sticky bit set, so the session cannot replace what root keeps there: the plugin directory,
# whose node_modules omp loads tools from, empty. run-cycle.sh checks all of it before each
# session.
RUN set -eu; \
    touch /home/ctx-agent/.bash_profile; \
    chown -R root:root /home/ctx-agent /workspace; \
    find /home/ctx-agent /workspace -type d -exec chmod 0755 {} +; \
    find /home/ctx-agent -type f -exec chmod 0644 {} +; \
    for dir in data state cache; do install -d -o root -g ctx-agent -m 1775 "/var/lib/omp/$dir/omp"; done; \
    install -d -o root -g root -m 0755 /var/lib/omp/data/omp/plugins
# omp's native module is embedded in the binary and extracted on its first run, which is this
# step: the Cloud Run job then starts without writing 111 MiB into its in-memory filesystem. The
# same run is the check that the skills are the session's own — `omp skill list` resolves skills
# exactly as a session in this directory does (packages/coding-agent/src/cli/skill-list.ts at the
# pinned release), so a config that does not reach them fails the build here. It runs as the
# session's user, with HOME already closed to it, so a build whose omp needs to write there
# fails here too.
USER ctx-agent
RUN set -eu; \
    ctx --version; \
    omp --version; \
    omp skill list --json > /tmp/skills.json; \
    jq -e '([.skills[] | select(.source == "custom:user") | .name] | sort) == ["ctx-memory", "ctx-memory-ingest"]' /tmp/skills.json > /dev/null; \
    rm /tmp/skills.json; \
    set -- "$XDG_DATA_HOME"/omp/natives/*/pi_natives.linux-x64-*.node; \
    test -f "$1"; \
    rm -rf "$XDG_STATE_HOME/omp/logs"
# The local state directory ctx would otherwise resolve from HOME: in the Cloud Run job ctx reads
# and writes through the server, so nothing of consequence lands here, but the directory is the
# container's own and never the share. A container that names its own CTX_STATE_DIR overrides it.
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
# The Cloud Run job passes no arguments: one execution is one cycle. The entrypoint keeps root
# only long enough to check what the session can touch and give it its directory on the share
# and the model key, hands the session to ctx-agent, and takes the directory back when the
# session ends.
ENTRYPOINT ["run-cycle.sh"]
