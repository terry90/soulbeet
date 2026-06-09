# syntax=docker/dockerfile:1
# Tier selector. Sourced from build/tiers/${TIER}.env in the beets-builder
# stage. Defaults to `light` so a plain `docker build .` preserves the
# pre-tiering image shape (FR-16-07).
ARG TIER=light

# Build Stage
FROM rust:1.91-bookworm AS builder

# Install build dependencies + Node 22 (distro Node 18 is too old for Tailwind v4 oxide)
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  curl \
  && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
  && apt-get install -y nodejs \
  && rm -rf /var/lib/apt/lists/*

# Install Dioxus CLI
RUN cargo install dioxus-cli@0.7.4 --locked

# Create app directory
WORKDIR /app

# Copy dependency files first for caching
COPY Cargo.toml Cargo.lock ./
COPY api/Cargo.toml api/
COPY desktop/Cargo.toml desktop/
COPY mobile/Cargo.toml mobile/
COPY ui/Cargo.toml ui/
COPY web/Cargo.toml web/
COPY lib/shared/Cargo.toml lib/shared/
COPY lib/soulbeet/Cargo.toml lib/soulbeet/

# Copy source code
COPY . .

# Install Tailwind dependencies (clean install to avoid npm optional dep bug on arm64)
RUN rm -rf node_modules package-lock.json && npm install

# Build the Tailwind CSS
RUN npx @tailwindcss/cli -i ./web/assets/input.css -o ./web/assets/tailwind.css

# Build the application
RUN dx bundle --package web --release

# Create empty directories for data and beets-plugin drop-in to be copied to
# runtime. The /empty_plugins copy guarantees /data/beets-plugins exists even
# when the user does not mount anything over it (Pitfall 4 mitigation).
RUN mkdir -p /empty_data /empty_plugins \
  && chmod 0755 /empty_data /empty_plugins

FROM python:3.11-slim-bookworm AS beets-builder

# Re-declare so the ARG is in scope within this stage (Dockerfile ARG scoping).
ARG TIER=light

RUN apt-get update && apt-get install -y --no-install-recommends \
  build-essential \
  && rm -rf /var/lib/apt/lists/*

# Create a virtual environment for beets
ENV VIRTUAL_ENV=/opt/venv
RUN python3 -m venv $VIRTUAL_ENV
ENV PATH="$VIRTUAL_ENV/bin:$PATH"

# Pull the tier manifest. It exports BEETS_EXTRAS, BEETS_PLUGINS, APT_EXTRAS,
# FFMPEG. The first two are consumed below; APT_EXTRAS and FFMPEG are reserved
# for plans 16-02 (chroma-native stage) and 16-03 (static ffmpeg COPY).
COPY build/tiers/${TIER}.env /tmp/tier.env

# Install beets and dependencies. Quote "beets${BEETS_EXTRAS}" so the shell
# does not glob the `[...]` extras list for medium / full tiers.
RUN pip install --no-cache-dir wheel
RUN . /tmp/tier.env \
  && pip install --no-cache-dir "beets${BEETS_EXTRAS}" requests musicbrainzngs

# Prune the venv. beets 2.11 declares numba/scipy as deps but never imports
# them (only lap + numpy in autotag/match.py). Stripping these and the venv
# bootstrap tools (pip/wheel/setuptools, never used at runtime) reclaims
# hundreds of MB. Also drop bytecode caches, package metadata, test suites,
# locale files, and strip native extensions.
RUN pip uninstall -y numba llvmlite scipy pip wheel setuptools \
  && find /opt/venv -type d -name __pycache__ -prune -exec rm -rf {} + \
  && find /opt/venv -type d -name '*.dist-info' -prune -exec rm -rf {} + \
  && find /opt/venv/lib/python3.11/site-packages -type d \( \
       -name tests -o -name test -o -name testing \
       -o -name docs -o -name doc -o -name examples \
       -o -name locale \
     \) -prune -exec rm -rf {} + \
  && find /opt/venv -type f \( -name '*.pyc' -o -name '*.pyo' \) -delete

# Template beets_config.yaml's `plugins:` line from BEETS_PLUGINS, and ensure
# `pluginpath:` is present. The sed pattern is anchored to `^plugins:` so the
# other top-level blocks (replaygain, musicbrainz, etc.) survive untouched.
COPY beets_config.yaml /tmp/beets_config.yaml
RUN . /tmp/tier.env \
  && sed -i "s|^plugins:.*|plugins: ${BEETS_PLUGINS}|" /tmp/beets_config.yaml \
  && (grep -q "^pluginpath:" /tmp/beets_config.yaml \
        || echo "pluginpath: /data/beets-plugins" >> /tmp/beets_config.yaml) \
  && cp /tmp/beets_config.yaml /beets_config.yaml

# rewrite shebang line in the executable script.
RUN sed -i '1s|^.*$|#!/usr/bin/python3|' $VIRTUAL_ENV/bin/beet

# Static ffmpeg binaries (consumed by chroma-native when FFMPEG=true).
# Declared before chroma-native so the bind-mount forward-reference resolves.
FROM docker.io/mwader/static-ffmpeg:8.0.1 AS ffmpeg-stage

# --- Chroma native deps (libchromaprint + libav* shared libs) ---
# Empty for TIER=light (APT_EXTRAS=""); populated for medium/full.
# Triplet derived from TARGETARCH (avoids needing dpkg-dev in the slim base).
FROM --platform=$TARGETPLATFORM debian:bookworm-slim AS chroma-native
ARG TIER
ARG TARGETARCH
COPY build/tiers/${TIER}.env /tmp/tier.env
RUN --mount=type=bind,from=ffmpeg-stage,target=/ffmpeg-src \
    . /tmp/tier.env \
  && case "${TARGETARCH}" in \
       amd64) TRIPLET="x86_64-linux-gnu" ;; \
       arm64) TRIPLET="aarch64-linux-gnu" ;; \
       *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
     esac \
  && mkdir -p /out/usr/bin /out/usr/lib/${TRIPLET} /out/usr/local/bin \
  && if [ -n "${APT_EXTRAS}" ]; then \
       apt-get update \
       && apt-get install -y --no-install-recommends ${APT_EXTRAS} \
       && rm -rf /var/lib/apt/lists/* \
       && cp /usr/bin/fpcalc /out/usr/bin/ \
       && ldd /usr/bin/fpcalc | awk '/=>/ {print $3}' | grep -v '^$' | sort -u | while read lib; do \
            real=$(readlink -f "$lib"); \
            [ -f "$real" ] || continue; \
            dest_dir="/out$(dirname "$real")"; \
            mkdir -p "$dest_dir"; \
            cp "$real" "$dest_dir/"; \
            soname=$(basename "$lib"); \
            realname=$(basename "$real"); \
            [ "$soname" = "$realname" ] || ln -sf "$realname" "$dest_dir/$soname"; \
          done ; \
     fi \
  && if [ "${FFMPEG}" = "true" ]; then \
       cp /ffmpeg-src/ffmpeg /out/usr/local/bin/ffmpeg \
       && cp /ffmpeg-src/ffprobe /out/usr/local/bin/ffprobe \
       && chmod 0755 /out/usr/local/bin/ffmpeg /out/usr/local/bin/ffprobe ; \
     fi

# --- RUNTIME STAGE ---
FROM gcr.io/distroless/python3-debian12

# Native binaries for chroma (fpcalc + libchromaprint + libav* shared libs)
# plus static ffmpeg + ffprobe at /usr/local/bin/ when TIER=full.
# Empty when TIER=light; populated for medium/full.
COPY --from=chroma-native /out/ /

# Copy beets virtual environment
COPY --from=beets-builder /opt/venv /opt/venv

# Working directory
WORKDIR /app

# Copy artifacts from builder
COPY --from=builder /app/target/dx/web/release/web /app/server
# Ship the templated beets_config.yaml from the beets-builder stage rather
# than the raw committed file, so the `plugins:` line matches the active tier.
COPY --from=beets-builder /beets_config.yaml /app/beets_config.yaml

# Copy empty data directory to ensure /data exists
COPY --from=builder /empty_data /data
# Pre-create /data/beets-plugins so beets does not silently no-op pluginpath
# lookups when the user has not mounted anything (Pitfall 4 mitigation).
COPY --from=builder /empty_plugins /data/beets-plugins

ENV PATH="/opt/venv/bin:/usr/local/bin:$PATH"

ENV PYTHONPATH="/opt/venv/lib/python3.11/site-packages"

ENV DATABASE_URL=sqlite:/data/soulbeet.db
ENV PORT=9765
ENV IP=0.0.0.0

# Expose the port
EXPOSE 9765

ENTRYPOINT ["/app/server/server"]