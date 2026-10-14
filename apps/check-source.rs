// Run by apps/check-source.sh inside a copy of ApplianceOS's console, as a module of applianceos-proxy's
// install tests, so it reaches the verifier and the archive reader the console itself uses.

use super::*;
use crate::source::Unverified;

const SOURCE: &str = "wiretap";

fn case(name: &str) -> Vec<u8> {
    let dir = std::env::var("WIRETAP_SOURCE_CASE").expect("run by apps/check-source.sh");
    std::fs::read(Path::new(&dir).join(name)).unwrap()
}

fn holding(root: &[u8]) -> (TempDir, Sources) {
    let dir = TempDir::new("wiretap-source");
    std::fs::write(dir.path().join(format!("{SOURCE}.pem")), root).unwrap();
    let sources = Sources::new(dir.path().join("image"), dir.path());
    (dir, sources)
}

fn untrusted_because(refused: Result<Verified, Unverified>, why: &str) {
    match refused {
        Err(Unverified::Untrusted { why: said, .. }) => assert_eq!(said, why),
        other => panic!("not refused as untrusted ({why}): {other:?}"),
    }
}

#[test]
fn the_signed_index_offers_both_apps_on_both_arches_each_image_in_the_archive() {
    let (_dir, sources) = holding(&case("root.pem"));
    let path = std::env::temp_dir().join(format!("wiretap-source-{}.tar", std::process::id()));
    std::fs::write(&path, case("signed.tar")).unwrap();
    let archive = read_archive(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let verified = sources
        .verify(&archive.index.unwrap(), &archive.signature.unwrap())
        .unwrap();
    assert_eq!(verified.source, SOURCE);
    let mut read: Vec<String> = verified
        .entries
        .iter()
        .map(|entry| format!("{} {}", entry.manifest.name, json!(entry.arch)))
        .collect();
    read.sort();
    assert_eq!(
        read,
        [
            r#"wiretap-gateway "amd64""#,
            r#"wiretap-gateway "arm64""#,
            r#"wiretap-server "amd64""#,
            r#"wiretap-server "arm64""#,
        ],
        "every entry is one this build reads"
    );
    for entry in &verified.entries {
        assert!(archive.images.contains_key(&entry.manifest.image.digest));
    }
}

#[test]
fn a_tampered_index_a_wrong_root_and_a_leaf_not_for_code_signing_are_refused() {
    let (_dir, sources) = holding(&case("root.pem"));
    assert_eq!(
        sources.verify(&case("tampered.json"), &case("index.json.sig")),
        Err(Unverified::Tampered)
    );
    untrusted_because(
        sources.verify(&case("index.json"), &case("server-auth.sig")),
        "its certificate is not for code signing",
    );
    let (_other, stranger) = holding(&case("other-root.pem"));
    untrusted_because(
        stranger.verify(&case("index.json"), &case("index.json.sig")),
        "its certificate is not issued under the source's root",
    );
}

#[tokio::test]
async fn a_host_admin_installs_each_app_from_the_archive_its_own_image_streamed() {
    let console = console().await;
    std::fs::write(
        console.dir.path().join(format!("app-sources/{SOURCE}.pem")),
        case("root.pem"),
    )
    .unwrap();
    let owner = console.harness.signed_in("owner", Role::HostAdmin).await;
    let archive = case("signed.tar");
    let response = console.upload(&owner, &archive).await;
    assert_eq!(response.status(), StatusCode::OK);
    let view = body_json(response).await;
    assert_eq!(view["source"], SOURCE);
    let apps = view["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2, "one per app for this box's arch: {view}");

    let index: Value = serde_json::from_slice(&case("index.json")).unwrap();
    for app in apps {
        let (name, version) = (app["name"].as_str().unwrap(), &app["version"]);
        let installed = console
            .harness
            .post("/api/app-archive/install")
            .cookie(&owner)
            .json(json!({"password": Harness::PASSWORD, "name": name, "version": version}))
            .send()
            .await;
        assert_eq!(installed.status(), StatusCode::NO_CONTENT, "{name}");
        let (request, image) = console.asked.lock().unwrap().pop().unwrap();
        let Request::Install { source, manifest } = request else {
            panic!("{name} asked for {request:?}");
        };
        assert_eq!(source, SOURCE);
        let signed = index["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["arch"] == json!(OWN_ARCH) && entry["manifest"]["name"] == name)
            .unwrap();
        let signed: AppManifest = serde_json::from_value(signed["manifest"].clone()).unwrap();
        assert_eq!(*manifest, signed);
        let tarball = format!("images/{}.tar", manifest.image.digest.replace(':', "-"));
        assert_eq!(
            image,
            case(&tarball),
            "{name}'s image is its own tarball, whole"
        );
        console.upload(&owner, &archive).await;
    }
}

#[tokio::test]
async fn a_host_admin_fetches_the_index_from_the_sources_url() {
    let console = console().await;
    let sources = console.dir.path().join("app-sources");
    std::fs::write(sources.join(format!("{SOURCE}.pem")), case("root.pem")).unwrap();
    let owner = console.harness.signed_in("owner", Role::HostAdmin).await;
    let served = Served::start().await;
    served.serve("index.json", &case("index.json"));
    served.serve("index.json.sig", &case("index.json.sig"));
    std::fs::write(sources.join(format!("{SOURCE}.url")), &served.base).unwrap();
    console.trust(&owner, &served).await;

    let response = console
        .harness
        .post(&format!("/api/app-sources/{SOURCE}/index"))
        .cookie(&owner)
        .send()
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let view = body_json(response).await;
    assert_eq!(view["apps"].as_array().unwrap().len(), 2, "{view}");
}
