# One image, two roles. The web server and the workers run the same binary,
# started with different flags, so a worker can never be a different build from
# the thing that was tested.

FROM rust:1.88-slim AS build
WORKDIR /src

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config \
 && rm -rf /var/lib/apt/lists/*

# Manifests first, so the dependency layer is cached until they actually
# change. Without this, editing one template rebuilds every crates.io crate.
COPY Cargo.toml Cargo.lock ./
COPY crates/app-core/Cargo.toml crates/app-core/
COPY crates/app-web/Cargo.toml crates/app-web/
COPY crates/app-integrations/Cargo.toml crates/app-integrations/
COPY crates/cluster-core/Cargo.toml crates/cluster-core/
COPY crates/cluster-local/Cargo.toml crates/cluster-local/
COPY crates/storage/Cargo.toml crates/storage/
COPY crates/server/Cargo.toml crates/server/
RUN for d in crates/*/; do \
        mkdir -p "$d/src" && echo "" > "$d/src/lib.rs"; \
    done \
 && mkdir -p crates/server/src && echo "fn main() {}" > crates/server/src/main.rs \
 && cargo build --release --locked -p server \
 && rm -rf crates

COPY . .
# Cargo stats the source, and the stub crates above were written after the real
# ones will be copied in; touch so the real sources are seen as newer.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 10001 app
COPY --from=build /src/target/release/server /usr/local/bin/server

# SQLite lives here. A volume, because a container's filesystem does not
# survive a redeploy and the price history is the point.
RUN mkdir -p /data && chown app:app /data
VOLUME ["/data"]

USER app
ENV APP_HOST=0.0.0.0 \
    APP_PORT=3000 \
    APP_DATABASE=/data/cluster.db
EXPOSE 3000 3001
ENTRYPOINT ["server"]
