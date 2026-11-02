# syntax=docker/dockerfile:1
#
# The server image: the static `ctx` of one ctx-memory release, and git, which is every program
# `ctx serve` and `ctx fetch` run. One image serves both (docs/deployment.md, "The server and the
# fetch job"): it has no CMD, so a container names its command, `serve …` or `fetch`.
#
# It runs as root (no USER line), because the share holding the data directory often lets only
# root write, and a server that cannot write there can store no window and move no marker.
#
# .github/workflows/images.yml builds it for each release and publishes it as
# ghcr.io/sjawhar/ctx-memory/server. By hand, from the repository root, with the values of that
# release's SHA256SUMS:
#   docker build --build-arg CTX_VERSION=<version> --build-arg CTX_SHA256=<sha256> images/server

# The release this image installs, and the SHA-256 of its x86_64 `ctx` archive as the release's
# SHA256SUMS lists it. A release asset can be replaced, so the archive is checked against this
# digest before anything in it is unpacked. Neither has a default: an image installs the release
# its build names, and no other.
ARG CTX_VERSION
ARG CTX_SHA256
ARG CTX_RELEASE_URL=https://github.com/sjawhar/ctx-memory/releases/download

# debian:trixie-slim, pinned by the digest of its multi-platform index (`crane digest
# debian:trixie-slim`, 2026-11-02), because a tag can be moved to another image.
FROM debian@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4 AS ctx
ARG CTX_VERSION
ARG CTX_SHA256
ARG CTX_RELEASE_URL
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# The archive holds `ctx` alone. Its digest is checked, then its listing, before anything is
# extracted, and the binary must report the version the build names.
RUN set -eu; \
    case "$CTX_VERSION" in "" | *[!0-9A-Za-z.-]*) echo "CTX_VERSION must be a release version such as 0.2.9: '$CTX_VERSION'" >&2; exit 1 ;; esac; \
    case "$CTX_SHA256" in *[!0-9a-f]*) echo "CTX_SHA256 must be lowercase hex: '$CTX_SHA256'" >&2; exit 1 ;; esac; \
    test "${#CTX_SHA256}" -eq 64 || { echo "CTX_SHA256 must be the archive's SHA-256 from the release's SHA256SUMS" >&2; exit 1; }; \
    curl -fsSL --retry 3 -o /tmp/ctx.tar.gz "${CTX_RELEASE_URL}/v${CTX_VERSION}/ctx-x86_64-unknown-linux-musl.tar.gz"; \
    printf '%s  %s\n' "$CTX_SHA256" /tmp/ctx.tar.gz | sha256sum --check --strict; \
    test "$(tar -tzf /tmp/ctx.tar.gz)" = ctx; \
    mkdir -p /out; \
    tar -xzf /tmp/ctx.tar.gz -C /out --no-same-owner ctx; \
    rm /tmp/ctx.tar.gz; \
    chmod 0755 /out/ctx; \
    test "$(/out/ctx --version)" = "ctx $CTX_VERSION"

# debian:trixie-slim
FROM debian@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG CTX_VERSION
LABEL org.opencontainers.image.source=https://github.com/sjawhar/ctx-memory \
      org.opencontainers.image.title=ctx-memory-server \
      org.opencontainers.image.description="The ctx-memory server and fetch job: ctx serve, ctx fetch" \
      org.opencontainers.image.licenses=MIT \
      org.opencontainers.image.version=${CTX_VERSION}
# git: the server and the fetch job clone the tree, and the server commits each write and pushes
# it. ca-certificates: the roots git and every HTTPS call ctx makes check the far side against.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ctx /out/ctx /usr/local/bin/ctx
# The local index (the SQLite store and its full-text index) is rebuilt from the windows on the
# share, so it belongs in the container's own filesystem, never on the share. A container that
# names its own CTX_STATE_DIR overrides this.
ENV CTX_STATE_DIR=/var/lib/ctx
RUN mkdir -p /var/lib/ctx
ENTRYPOINT ["ctx"]
