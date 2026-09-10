<!--
Release notes template for WireTAP-Server. Matches the WireTAP desktop's house
style, so a reader who follows both repositories sees one voice.

`release.yml` creates the draft with a placeholder body pointing at the
CHANGELOG. Replace it with a filled-in copy of this file before publishing:

    gh release edit vX.Y.Z --notes-file <your-filled-in-copy>

Delete any section you have nothing for — an empty "Fixed" heading reads worse
than no heading. Delete these comments too.

## How to write it

**Audience: whoever runs this on a machine.** Not the desktop's users and not
this repository's contributors. Ask of every line: does an operator do something
differently because of it? If not, it belongs in the CHANGELOG, not here.

**Highlights is prose, not bullets.** One short paragraph, occasionally two.
Say what is now possible that was not, and name the catch it removes — the
desktop's best notes lead with the problem ("Discovering a Modbus device no
longer needs a register map — which was the catch, because the register map is
the thing you are trying to find"). If the release breaks something, say so here
in bold and point at the section that explains it.

**Bullets are outcomes, not changes.** "A multi-bus GVRET offers all of its
buses" — not "fixed bus enumeration in the probe". Name the thing the reader
sees.

**Bold whatever demands an action**, and put the action in the sentence:
"**Re-probe any GVRET you set up while its bus was live**", "**Remove any
workaround you made in Settings.**" A reader skimming bold should find every
job the upgrade gives them.

**Say the version something is fixed relative to** when the bug shipped in a
named release, so a reader can tell whether it ever affected them.

**No marketing, no emoji, no headings beyond these.** Australian English.
Version numbers and file paths in backticks.
-->

## Highlights

<!-- One paragraph. What can an operator now do, or stop worrying about? -->

### New

<!-- Capabilities that did not exist. Each line an outcome an operator sees. -->

-

### Changed

<!-- Behaviour that differs from the previous release. Bold anything that
     needs an action, and say what the action is. Breaking changes lead. -->

-

### Fixed

<!-- Bold the ones where a reader must go and check something, e.g. re-run a
     capture, re-check an archive, or redo a configuration. -->

-

### Upgrading

<!-- Only when the upgrade is not `apt install ./wiretap-server_X.Y.Z_arch.deb`
     and a restart. Schema migrations, config changes, wire-protocol version
     bumps that need both ends moved together, anything that must be done in a
     particular order. -->

---

**Packages:** `wiretap-server_X.Y.Z_amd64.deb`, `wiretap-server_X.Y.Z_arm64.deb`,
verified against `SHA256SUMS`.
**Gateway image:** `ghcr.io/wired-square/wiretap-backend:X.Y.Z`

See the [CHANGELOG](https://github.com/Wired-Square/WireTAP-Server/blob/main/CHANGELOG.md)
for details.
