use std::fs;
use std::path::Path;

/// The product's version is the root workspace's, which the release tags and the images carry.
fn main() {
    wslib_build_id::emit();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../Cargo.toml");
    println!("cargo::rerun-if-changed={}", root.display());
    let manifest: toml::Table = fs::read_to_string(&root)
        .unwrap_or_else(|e| panic!("{}: {e}", root.display()))
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", root.display()));
    let version = manifest["workspace"]["package"]["version"]
        .as_str()
        .expect("the root workspace names its version");
    println!("cargo::rustc-env=WIRETAP_VERSION={version}");
}
