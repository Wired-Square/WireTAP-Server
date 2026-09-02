# Build context is the WORKSPACE ROOT (see docker-compose.yml), because the
# crate is a workspace member and cargo needs the root manifest and lockfile.
# The capture schema now lives inside this crate, so nothing reaches across
# directories any more.

# --- admin UI (static SPA served at /admin) ---
FROM node:22-bookworm-slim AS admin-ui
WORKDIR /ui
COPY crates/wiretap-backend/admin-ui/package.json crates/wiretap-backend/admin-ui/package-lock.json* ./
RUN npm install
COPY crates/wiretap-backend/admin-ui/ ./
RUN npm run build

# --- backend binary ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
# The whole crates/ tree: cargo resolves every workspace member's manifest, so
# copying only this one would fail as soon as a second crate lands.
COPY crates/ crates/
RUN cargo build --release -p wiretap-backend

# --- runtime ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wiretap-backend /usr/local/bin/wiretap-backend
COPY --from=admin-ui /ui/dist /usr/share/wiretap-admin
EXPOSE 8423 9323
HEALTHCHECK --interval=15s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:8423/v1/health || exit 1
ENTRYPOINT ["wiretap-backend"]
