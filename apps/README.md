# ApplianceOS apps

| App | What |
| --- | --- |
| [wiretap-gateway](wiretap-gateway/README.md) | the gateway with its own Postgres and TimescaleDB |
| [wiretap-server](wiretap-server/README.md) | the capture daemon and its web UI |

Each `make-image.sh` writes one `docker load` tarball per architecture under
`target/apps/`. This page is about shipping them to a box as a signed source.

## The source archive

ApplianceOS installs a signed app from an archive (the console's Apps ›
Install): a tar of `index.json`, its detached CMS signature `index.json.sig`,
and each image at `images/sha256-<hex>.tar`. The index lists every app and
architecture with its full manifest, pinned to the image's OCI manifest
digest, which is the image ID the box's containerd store gives it on load.
The format is ApplianceOS's `scripts/make-app-archive.sh`'s.

```sh
apps/wiretap-gateway/make-image.sh && apps/wiretap-server/make-image.sh
apps/make-source.sh pack target/apps/wiretap-apps-unsigned.tar
```

`pack` takes both apps on both architectures at the crate's version, or the
tarballs you name. It refuses an image with no manifest label, one whose
label and OCI version disagree, and two images of the same app, version and
architecture. The index names the source `wiretap` (`SOURCE=` to change it);
a box trusts it once its root is added in Apps › Sources under that name.

## Signing a release

The source's key never reaches CI or this repository. A maintainer signs, on a
machine holding the key:

1. Download `app-source-unsigned` from the release's workflow run and check
   `SHA256SUMS`.
2. Sign with the source's Code Signing leaf: a certificate under the source's
   root with `extendedKeyUsage=codeSigning` and `keyUsage=digitalSignature`,
   on an ECDSA P-256 or P-384 key or RSA 2048–8192. Name any intermediate in
   `chain.pem`; the box refuses one whose extended key usage leaves out
   `codeSigning`, even one with none. OpenSSL 3 is needed (`OPENSSL=` to
   choose one).

   ```sh
   apps/make-source.sh sign wiretap-apps-<version>-unsigned.tar \
     wiretap-apps-<version>.tar leaf.pem leaf.key [chain.pem]
   ```

   The box checks the chain at the signing time the signature carries, so the
   leaf must be valid when it signs.
3. Install the signed archive on a test box before it goes anywhere else.

## Testing with throwaway keys

```sh
apps/make-source.sh test-keys target/apps/test-keys
apps/make-source.sh sign target/apps/wiretap-apps-unsigned.tar \
  target/apps/wiretap-apps-test.tar target/apps/test-keys/leaf.pem target/apps/test-keys/leaf.key
```

The root and leaf last 30 days. Keep them under `target/`, which git ignores,
and add `root.pem` to a test box only.

## Checking against ApplianceOS

```sh
apps/check-source.sh <ApplianceOS checkout> [<unsigned.tar>]
```

It signs the archive with throwaway keys and runs
[check-source.rs](check-source.rs) as a test inside a copy of the checkout's
console, through the code a box runs: the archive reader, the signature
check, the upload and install routes, and a fetch from a source's URL. It
also proves that a tampered index, a root the box does not hold, and a leaf
not for code signing are refused. It needs the private ApplianceOS repository
and the chassis, so CI does not run it; `APPLIANCEOS_REV` picks the commit.
