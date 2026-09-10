//! The commit stamp read back as `env!("WIRETAP_BUILD_ID")` by `lib.rs`.
//! Everything it does, and why, is in `wiretap-build-id`.

fn main() {
    wiretap_build_id::emit();
}
