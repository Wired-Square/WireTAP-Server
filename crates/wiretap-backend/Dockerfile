# Build context is the WORKSPACE ROOT (see docker-compose.yml), because the
# crate is a workspace member and cargo needs the root manifest and lockfile.

# --- admin UI (static SPA served at /admin) ---
FROM node:22-bookworm-slim AS admin-ui
WORKDIR /ui
COPY crates/wiretap-backend/admin-ui/package.json crates/wiretap-backend/admin-ui/package-lock.json ./
# ci, not install: install re-resolves the `^` ranges against the registry, so
# the image can drift off the committed lockfile.
RUN npm ci
COPY crates/wiretap-backend/admin-ui/ ./
RUN npm run build

# --- backend binary ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Only what cargo compiles. Copying all of crates/ would put the admin-ui
# sources, the READMEs and the compose files in the layer that gates
# `cargo build`, so editing a .tsx would recompile all ~130 crates.
#
# Every workspace member has to be here, because `members = ["crates/*"]` is a
# glob and the workspace inside this image is whatever was copied — cargo loads
# every member's manifest, and every path dependency's, before it builds
# anything. Copying only the backend left `wiretap-model` and
# `wiretap-ingest-proto` missing, which is a build failure and not a smaller
# build.
#
# What each member needs differs, and the rule is below: a *dependency* gets its
# real source, a member that is only a member gets a stub. The capture server is
# the latter — present so the workspace matches Cargo.lock and `--locked` can
# mean something, never compiled.
#
# Nothing here notices when a crate is added, so adding one breaks this build.
# `ci.yml`'s image job is what catches that, and it exists because this went
# unnoticed for two days.
COPY crates/wiretap-model/Cargo.toml        crates/wiretap-model/
COPY crates/wiretap-model/src               crates/wiretap-model/src
COPY crates/wiretap-ingest-proto/Cargo.toml crates/wiretap-ingest-proto/
COPY crates/wiretap-ingest-proto/src        crates/wiretap-ingest-proto/src
# The capture server is a member and not a dependency: cargo has to load its
# manifest and find *a* target, and never builds it. Its real source is the
# largest and fastest-moving in the workspace, and putting it here would gate
# the backend's build on it — every edit to the capture path rebuilding the
# gateway's whole dependency graph, which is the cost the comment above exists
# to avoid. A stub satisfies target discovery, and `--locked` still means what
# it says: resolution reads manifests, not sources.
COPY crates/wiretap-server/Cargo.toml       crates/wiretap-server/
# compile_error! is inert while nothing builds this crate, and names the file
# the instant something does - a `--workspace` here would otherwise produce a
# real binary called wiretap-server that does nothing, and say not a word.
RUN mkdir -p crates/wiretap-server/src \
    && printf '%s\n%s\n' \
       'compile_error!("stub: the gateway image fabricates this target; COPY the real source to build it");' \
       'fn main() {}' > crates/wiretap-server/src/main.rs
COPY crates/wiretap-backend/Cargo.toml      crates/wiretap-backend/
COPY crates/wiretap-backend/src             crates/wiretap-backend/src
COPY crates/wiretap-backend/schema          crates/wiretap-backend/schema
# --locked for the same reason `npm ci` is used above: a build that may quietly
# re-resolve is a build that can drift off what was committed and tested.
RUN cargo build --release --locked -p wiretap-backend

# --- runtime ---
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wiretap-backend /usr/local/bin/wiretap-backend
COPY --from=admin-ui /ui/dist /usr/share/wiretap-admin
# What associates the GHCR package with this repository. Without it the package
# Actions creates stands alone, so the one-time "flip it to Public" that
# release.yml documents is not reachable from the repository's own page.
# Last, not next to the FROM: an instruction spliced in above the apt layer
# changes that layer's cache key, and this label is worth nobody's rebuild.
LABEL org.opencontainers.image.source=https://github.com/Wired-Square/WireTAP-Server
EXPOSE 8423 9323
HEALTHCHECK --interval=15s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:8423/v1/health || exit 1
ENTRYPOINT ["wiretap-backend"]
