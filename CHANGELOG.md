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

### Notes

- Nothing is packaged yet. The `.deb` files and a published multi-architecture gateway
  image arrive with the first tagged release; until then the gateway is built from source
  by its own Compose stack, as before.
