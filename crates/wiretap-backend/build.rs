//! The commit stamp read back as `env!("WIRETAP_BUILD_ID")` by `main.rs`.
//! Everything it does, and why, is in `wiretap-build-id`.
//!
//! In the container build there is no `.git`, so the value comes from the
//! `WIRETAP_BUILD_ID` build argument the Dockerfile turns into an environment
//! variable — which is the case that fallback exists for.

fn main() {
    wiretap_build_id::emit();
}
