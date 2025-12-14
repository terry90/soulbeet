# Build Stage
FROM rust:1.91-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  nodejs \
  npm

# Install Dioxus CLI
RUN cargo install dioxus-cli

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

# Install Tailwind dependencies
RUN npm install

# Build the Tailwind CSS
RUN npx @tailwindcss/cli -i ./web/assets/input.css -o ./web/assets/tailwind.css

# Build the application
RUN dx bundle --package web --release

# Create an empty directory for data to be copied to runtime
RUN mkdir -p /empty_data

# Beets Build Stage
FROM python:3.11-slim-bookworm AS beets-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
  build-essential \
  && rm -rf /var/lib/apt/lists/*

# Create a virtual environment for beets
ENV VIRTUAL_ENV=/opt/venv
RUN python3 -m venv $VIRTUAL_ENV
ENV PATH="$VIRTUAL_ENV/bin:$PATH"

# Install beets and dependencies
RUN pip install --no-cache-dir wheel
RUN pip install --no-cache-dir beets requests musicbrainzngs

# Runtime Stage
FROM gcr.io/distroless/python3-debian12

# Copy ffmpeg from static image
# COPY --from=docker.io/mwader/static-ffmpeg:8.0.1 /ffmpeg /usr/local/bin/ffmpeg
# COPY --from=docker.io/mwader/static-ffmpeg:8.0.1 /ffprobe /usr/local/bin/ffprobe

# Copy beets virtual environment
COPY --from=beets-builder /opt/venv /opt/venv

# Set environment variables for Python/Beets
ENV VIRTUAL_ENV=/opt/venv
ENV PATH="/opt/venv/bin:/usr/local/bin:$PATH"
ENV PYTHONPATH="/opt/venv/lib/python3.11/site-packages"

# Working directory
WORKDIR /app

# Copy artifacts from builder
COPY --from=builder /app/target/dx/web/release/web /app/server
COPY beets_config.yaml /app/beets_config.yaml

# Copy empty data directory to ensure /data exists
COPY --from=builder /empty_data /data

# Set environment variables
ENV DATABASE_URL=sqlite:/data/soulbeet.db
ENV PORT=9765
ENV IP=0.0.0.0

# Expose the port
EXPOSE 9765

ENTRYPOINT ["/app/server/web"]
