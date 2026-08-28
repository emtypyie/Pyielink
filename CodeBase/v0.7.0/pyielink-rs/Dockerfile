# PYIELINK FRAMEWORK — v0.7.0 container image
#
# Builds the Rust peer binary and bundles the Node data layer + ffmpeg so the
# same image can run either the HOST (shares its screen) or the VIEWER
# (watches a remote screen). Pick the mode with the container command:
#   docker compose up -d host
#   docker compose run --rm -e PYIELINK_HOST=<ip> view
#
# Screen capture:
#   - Windows hosts use the bundled DXGI helper (Desktop Duplication).
#   - Linux hosts need an X session; the image ships Xvfb for headless viewers.
#     (Host capture on bare Linux requires x11grab support — see README.)

FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs && cargo build --release
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:stable-slim
LABEL org.opencontainers.image.title="pyielink-framework"
LABEL org.opencontainers.image.description="PYIELINK FRAMEWORK — peer-to-peer remote machine access over encrypted internet, with live screen streaming"
LABEL org.opencontainers.image.vendor="EMTYPYIE&CO"
LABEL org.opencontainers.image.version="0.7.0"

RUN apt-get update && apt-get install -y --no-install-recommends \
      nodejs npm ffmpeg xvfb ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /src/target/release/pyielink /usr/local/bin/pyielink
COPY datalayer ./datalayer
RUN cd /app/datalayer && npm install --omit=dev

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV PYIELINK_HOME=/data
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["host"]
