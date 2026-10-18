//! The commit stamp read back by `build_version!` in `main.rs`.
//!
//! In the container build there is no `.git`, so the value comes from the
//! `WSLIB_BUILD_ID` build argument the Dockerfile turns into an environment
//! variable.

fn main() {
    wslib_build_id::emit();
}
