# Corrections to published compliance documents

Anything wrong that has already been published is corrected here in the open,
with what was wrong, what it should have said, and how to tell the two apart.
Files are never quietly replaced: a compliance document that changes without
saying so is worth less than one that is wrong and says so.

---

## v2.13.0: the PDF manuals attached to the release embedded system fonts

**What was wrong.** From v2.5.0 to v2.13.0 the release package carried the six
manuals as Markdown only; the PDFs had gone missing when the release build was
automated. On 2026-11-01 the six PDFs of v2.13.0 were attached to the GitHub
release, printed from the v2.13.0 text. That first set, and a second set the same
day, embedded fonts of the machine that printed them: subsets of Arial, Trebuchet
MS and Segoe Fluent Icons in the first, and in the second the outlines of one
warning sign (U+26A0, in the two user manuals) drawn from Segoe UI Symbol. None
of them was named in ATTRIBUTION.md.

**Scope.** The six PDF manuals attached to the v2.13.0 GitHub release only. The
text is the v2.13.0 text in all three sets, except that the third replaces the
warning sign with words ("with a warning sign"). The programs, the zip and
SHA256SUMS.txt of v2.13.0 are unchanged.

**Corrected on 2026-11-01.** The third set embeds only Open Sans and Source Code
Pro, the two fonts of the documentation tool, named with their licences in
ATTRIBUTION.md.

| Set | Installatie | Installation | User-Manual | User-Manual-EN | Technische-Referentie | Technical-Reference |
|---|---|---|---|---|---|---|
| 1, attached and replaced the same day | `62ca32bd...` | `59d49f5d...` | `c0d0bc86...` | `1b83538a...` | `230d7365...` | `02d690fc...` |
| 2, attached and replaced the same day | `cd8a7d26...` | `bf9f5943...` | `779650c5...` | `11e10089...` | `b3120e01...` | `26edd148...` |
| 3, current | `cd3038fa...` | `c65e224e...` | `51f560b8...` | `d1a18673...` | `370e2a51...` | `fb9834ba...` |

The PDFs were attached after the release, so the release's own SHA256SUMS.txt
does not list them; this is where their checksums are. The full SHA-256 of the
current set:

```
cd3038faf4186cef1fae6e623cdba7fb31fc7cfe777fe8ce815ffe4b5e8b6626  Installatie.pdf
c65e224e5ebf92fd49017fca542b1d98176472b74b0c85dc235de018d14417a5  Installation.pdf
51f560b87be591fee3d7ef119516e1d1fda81ec64e21c900dbdda2df567ed9e7  User-Manual.pdf
d1a186730eb6e59de721f316b875819f58eb5ad84551425b4216c5198fe7831a  User-Manual-EN.pdf
370e2a510c20d6976e4c40a80badfabee0dbb824ea67fa896e4b1fb0fe526e47  Technische-Referentie.pdf
fb9834ba06e9fcc03d41ee8ad90ac22ff5548c6c4fa15822113984dbbf94f4f2  Technical-Reference.pdf
```

**What a reader should do.** If you downloaded a PDF manual of v2.13.0 on
2026-11-01, take it again from the release.

**What changed so it cannot recur.** From v2.13.1 the release build makes the six
PDFs itself and refuses one that embeds any font other than the two above, by
name or as drawn outlines (`scripts/pdf-fonts.py`, run when the package is built
and again when it is published). Both directions were verified by giving it the
first two sets and watching it refuse them.

## v2.10.0: `sbom.spdx.json` named the wrong version for this project's own components

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
identity *and* by its declared licence, concluded licence and origin (local
crates included) and refuses a bill of materials that is not in the fixed
order. Both were verified by breaking them on purpose and watching the gate go
red.

**One limitation, stated plainly.** The checksum above is of the file as it
stood in the repository at the v2.10.0 release, taken from version control.
There is no v2.10.0 tag in this repository (the release is published from a
mirror) so this is the same file, identified from the source side rather than
from a published artefact.
