# syntax=docker/dockerfile:1.28
#
# The Tordo site generator as a self-contained image. It bundles the generator
# plus its templates/i18n/static/examples, so the only thing it needs at run
# time is a catalog (config.toml + content/ + cache/) mounted at /catalog:
#
#   docker run --rm -v "$PWD:/catalog" ghcr.io/ogarcia/tordo
#
# reads /catalog/config.toml, /catalog/content/ and /catalog/cache/, and writes
# the built site to /catalog/dist/ (override with -e CATALOG_DIR / CATALOG_OUT).
# A TMDB token is only needed for entries not already cached:
#
#   docker run --rm -e TMDB_READ_TOKEN=... -v "$PWD:/catalog" ghcr.io/ogarcia/tordo
#
# Local build (compiles from source):
#   podman build -t tordo:dev .
#
# CI build (binary pre-compiled into artifacts/<arch>/, arch = amd64|arm64 to
# match Docker's TARGETARCH; buildx picks the right one per platform):
#   podman build --build-arg BUILDER=prebuilt .

# ── Stage 1a: compile from source (default local path) ──────────────────────

# ARG before the first FROM so it is a global build argument. Default 'builder'
# (local compile); CI passes --build-arg BUILDER=prebuilt to use artifacts/.
ARG BUILDER=builder

# Base-image version. Default mirrors versions.env (the single source of truth);
# CI overrides it with --build-arg. Declared before the first FROM so it applies
# to every FROM below.
ARG ALPINE_VERSION=3.24
ARG RUST_VERSION=1

FROM docker.io/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder

# musl-dev: C headers for ring (pulled in by rustls via minreq's https feature).
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY . .
RUN cargo build --release --locked && \
    cp target/release/tordo /usr/bin/tordo && \
    strip /usr/bin/tordo

# ── Stage 1b: pre-built binary injected from CI (artifacts/ in context) ──────

# TARGETARCH (amd64/arm64) is set automatically by buildx per target platform;
# re-declare it here to use it in COPY. The artifacts/<arch>/ layout is produced
# by the container.yml build matrix.
FROM scratch AS prebuilt
ARG TARGETARCH
COPY --chmod=0755 artifacts/${TARGETARCH}/tordo /usr/bin/tordo

# ── Stage 1c: indirection — points to 'builder' or 'prebuilt' via build arg ──

FROM ${BUILDER} AS bins

# ── Stage 2: runtime ────────────────────────────────────────────────────────

FROM docker.io/alpine:${ALPINE_VERSION} AS runtime

# ca-certificates: TLS roots for fetching TMDB metadata over HTTPS.
RUN apk add --no-cache ca-certificates

COPY --from=bins /usr/bin/tordo /usr/bin/tordo

# The generator reads its templates/i18n/static/examples from the working
# directory, so bundle them and run from there. The catalog (config, content,
# cache) comes from CATALOG_DIR at run time.
WORKDIR /app
COPY templates ./templates
COPY static ./static
COPY examples ./examples
COPY i18n ./i18n
COPY config.example.toml ./config.example.toml

ENV CATALOG_DIR=/catalog
ENV CATALOG_OUT=/catalog/dist

ENTRYPOINT ["/usr/bin/tordo"]
