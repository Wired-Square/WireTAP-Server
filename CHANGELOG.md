# Changelog

All notable changes to this project are documented here. Entries go under
`[Unreleased]` until a release is cut.

## [Unreleased]

### Added

- Split the capture server and the database gateway out of the WireTAP desktop repo into
  this one, preserving their history back to the original 2026-01-13 `candor-server`
  commit. The desktop repo is unchanged; its copies stay in place until the Rust port is
  proven in the field.
- MIT `LICENSE` and a DEP-5 `debian/copyright`. Neither program had a licence file before.
- Cargo workspace: dependency versions and package metadata are declared once at the root,
  so two crates cannot end up on different versions of a crate that passes types between
  them.

### Changed

- The capture schema moved into the gateway crate at
  `crates/wiretap-backend/schema/init_schema.sql`. It used to sit beside the Python server
  and be reached with a cross-directory `include_str!`, which forced the Docker build
  context to be the whole desktop repo and required an allowlist `.dockerignore` to
  re-admit one file. The gateway is now the only thing that needs the schema, so the
  Dockerfile builds from an ordinary workspace root and `.dockerignore` is a plain
  deny-list.
- The capture server will **forward to the gateway only**. Its direct-to-PostgreSQL sink is
  not being ported: the gateway owns the database, as its own documentation has always
  said. A configuration with `[postgres].enable = true` will be refused at startup rather
  than ignored, because silently ignoring it would drop every frame an operator believes is
  being archived. `tools/migrate_to_timescale.py` remains for moving an existing archive.

### Fixed

- `.dockerignore`'s secrets patterns were root-anchored and so matched nothing below the
  top level — Docker, unlike git, does not treat a slash-free pattern as any-depth. The
  `.env` the READMEs tell you to create sits at `crates/wiretap-backend/.env` and was
  therefore inside the build context, alongside a `COPY crates/` that would have carried it
  into the image. Both halves are fixed: the patterns are `**/`-prefixed, and the Dockerfile
  copies only the manifest, `src/` and `schema/`. Narrowing the copy also keeps the admin-ui
  sources out of the layer that gates `cargo build`, where editing a `.tsx` recompiled every
  dependency.
- Folding the gateway crate's `.gitignore` into the root dropped `docker-compose.override.yml`
  and `*.tsbuildinfo`. Restored, and the patterns are now path-independent so a second crate
  is covered before it exists.
- Six documentation links and paths left dangling by the move. The pass that rewrote them
  originally only matched references that *named* the moved directory, which is why some
  survived.
- Two clippy warnings inherited from the gateway crate, and a one-off `rustfmt` pass over
  it — it had never been formatted, having lived in a repo whose CI only built the desktop
  app. `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` all pass.

### Notes

- Nothing is packaged yet. The `.deb` files and a published multi-architecture gateway
  image arrive with the first tagged release; until then the gateway is built from source
  by its own Compose stack, as before.
- The Docker stack is verified on Apple Silicon (native arm64): image builds in ~46 s,
  both services healthy, `smoke_test.sh` 29/29 and the ingest conformance suite 12/12.
  `smoke_test.sh` needs a seeded database first — see the gateway's README.
