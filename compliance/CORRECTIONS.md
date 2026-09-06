# Corrections to published compliance documents

Anything wrong that has already been published is corrected here in the open,
with what was wrong, what it should have said, and how to tell the two apart.
Files are never quietly replaced: a compliance document that changes without
saying so is worth less than one that is wrong and says so.

---

## v2.10.0 — `sbom.spdx.json` named the wrong version for this project's own components

**What was wrong.** The software bill of materials listed the ten crates that
make up ThetisLink itself at version **2.9.1**, while the release was **2.10.0**.
Third-party dependencies were unaffected: every external package, its version,
its licence and its origin were listed correctly.

**Why it was not noticed.** The release gate compared only packages carrying a
package URL, and a local crate does not have one. This project's own components
therefore fell outside the check entirely, and the file could say anything about
them without anything objecting.

**Scope.** Ten package entries, `versionInfo` field only. No licence statement,
no copyright notice and no origin was affected, for our own code or for anyone
else's. Nothing in the file understated an obligation to a third party.

**Corrected on 2026-09-01.**

| | SHA-256 | Size |
|---|---|---|
| As published with v2.10.0 | `8ed1a087914bb8ff9862aea4f4737385d305c9337c2d261d8a3fd1ead970011f` | 893.088 bytes |
| Corrected | `ab5b629d64630e81ffbfd8835800516c0c68a305bddc5b99cf7c2976c9b87a48` | 893.404 bytes |

The corrected file also has its package, file and relationship lists in a fixed
order, so the two differ in more than the ten versions. The ordering carries no
meaning in SPDX; it exists so that the next change to this file is legible
instead of being buried in forty-eight thousand moved lines.

**What a reader should do.** If you hold a copy of the v2.10.0 SBOM and rely on
it, take the corrected file from the repository. If you rely only on the
third-party sections, the copy you have is accurate.

**What changed so it cannot recur.** The gate now compares every package by
identity *and* by its declared licence, concluded licence and origin — local
crates included — and refuses a bill of materials that is not in the fixed
order. Both were verified by breaking them on purpose and watching the gate go
red.

**One limitation, stated plainly.** The checksum above is of the file as it
stood in the repository at the v2.10.0 release, taken from version control.
There is no v2.10.0 tag in this repository — the release is published from a
mirror — so this is the same file, identified from the source side rather than
from a published artefact.
