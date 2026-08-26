# pyielink connector client — resilient container build
#
# Build:      docker build -t pyielink-connector .
# First run:  docker compose run --rm connector   (interactive: license + password once)
# Daemon run: PYIELINK_TARGET=bob@192.168.1.50 docker compose up -d
#
# The container keeps the session credential in the named volume
# `connector-data` (PYIELINK_HOME=/data), so after one interactive login the
# daemon reconnects with zero input. compose.yml sets restart: unless-stopped
# and the entrypoint retries forever with backoff.

FROM rust:1-slim AS build
WORKDIR /src
# cache dependency build with a stub main, then layer real sources on top
COPY Cargo.toml ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && cargo build --release
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:stable-slim
LABEL org.opencontainers.image.title="pyielink-connector"
LABEL org.opencontainers.image.description="pyielink remote access framework - connector client"
COPY --from=build /src/target/release/pyielink /usr/local/bin/pyielink
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENV PYIELINK_HOME=/data
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
