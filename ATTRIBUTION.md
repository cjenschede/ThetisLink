# Attribution and Provenance

This project builds upon and interoperates with the Thetis SDR software,
originally developed within the OpenHPSDR ecosystem as a continuation of
FlexRadio's PowerSDR.

## Upstream lineage

- **FlexRadio PowerSDR** (2004–2009) — original open-source SDR application;
  © FlexRadio Systems, licensed under GPL
- **OpenHPSDR Thetis** (2010–present) — PowerSDR continuation for OpenHPSDR
  hardware; originally maintained by Doug Wigley (W5WC) from 2010 through
  approximately 2020, continued by Richard Samphire (MW0LGE) from 2019 to
  the present
- **`ramdor/Thetis`** — the upstream repository that this project's
  Thetis fork (`cjenschede/Thetis`, branch `thetislink-tci-extended`)
  derives from

## Contributors to the upstream Thetis lineage

The following contributors are acknowledged for their work on the upstream
Thetis lineage and associated components (list compiled from in-file
copyright headers, the FlexRadio/OpenHPSDR project records, and the formal
notice issued by Richard Samphire in 2026 concerning attribution of the
Thetis lineage):

- Richard Samphire (MW0LGE)
- Warren Pratt (NR0V)
- Laurence Barker (G8NJJ)
- Rick Koch (N1GP)
- Bryan Rambo (W4WMT)
- Chris Codella (W2PA)
- Doug Wigley (W5WC)
- FlexRadio Systems
- Richard Allen (W5SD)
- Joe Torrey (WD5Y)
- Andrew Mansfield (M0YGG)
- Reid Campbell (MI0BOT)

## Scope of this project's derivative relationship

### `cjenschede/Thetis` fork (branch `thetislink-tci-extended`)

This fork is a direct derivative of `ramdor/Thetis` under GPL-2.0-or-later.
All upstream copyright notices, license text, and per-file attribution
are preserved unchanged. Modifications made in this fork are marked in the
affected source files with in-file "Modified by" notices consistent with
GPL §2(a) requirements.

### ThetisLink (this Rust workspace)

ThetisLink is a separate Rust application that communicates with Thetis
over the TCI WebSocket protocol and the Kenwood-style CAT TCP interface.
It runs as an independent process and does not statically or dynamically
link any Thetis code into its binaries.

The TCI protocol is an open, documented protocol maintained by
Expert Electronics (ExpertSDR2 lineage) and implemented independently here.

Where individual Rust source files were informed by Thetis internal
behaviour during authoring, they are licensed under GPL-2.0-or-later
alongside the rest of this workspace, and carry an `SPDX-License-Identifier`
header accordingly.

The description of ThetisLink as a separate process not linked to Thetis is
a factual statement about the binary structure of the deployed software; it
is not a claim that ThetisLink is unrelated to Thetis, nor a disavowal of
any derivative-work relationship. Individual file-level derivative status
is addressed file-by-file through SPDX-headers and the GPL-2.0-or-later
licensing of the entire Rust workspace.

## Dual-licensing statement regarding MW0LGE contributions

The upstream Thetis tree includes a `LICENSE-DUAL-LICENSING` file in which
Richard Samphire (MW0LGE) reserves the right to license his own
contributions under terms other than the GPL, in addition to the GPL
granted in the main `LICENSE`. That statement applies only to code
originally written by Richard Samphire or modifications made by him, and
it does not restrict any rights granted to recipients under the GPL.

Code contributed by others is unaffected by this dual-licensing statement
and remains licensed under its original terms (GPL-2.0-or-later).

This project does not assert any rights beyond those granted by the GPL
over contributions by Richard Samphire or any other upstream contributor.

## Third-party dependency licenses

Runtime dependencies are inventoried in `compliance/sbom.spdx.json`
(SPDX 2.3 SBOM) and their license texts bundled in
`compliance/THIRD-PARTY-LICENSES.html`. All runtime dependencies are
licensed under terms compatible with GPL-2.0-or-later.

One dependency, `epaint_default_fonts`, ships Ubuntu Font-Family assets
under the **Ubuntu Font License 1.0 (UFL-1.0)**. UFL-1.0 is a Canonical
FOSS font licence compatible with GPL. Because UFL has no standard SPDX
identifier, its text is not automatically included in the generated
THIRD-PARTY-LICENSES bundle; we therefore ship the licence text
separately as `compliance/licenses/UFL-1.0.txt` (sourced verbatim from
the upstream crate's `fonts/UFL.txt`). This licence applies solely to
the bundled Ubuntu font files.

## Third-party protocol and specification references

- **HPSDR / OpenHPSDR Protocol 2** — public protocol specification
  maintained by TAPR / OpenHPSDR; implemented here from public documentation
- **TCI (Transceiver Control Interface)** — public protocol specification
  maintained by Expert Electronics
- **Kenwood TS-2000 CAT** — base CAT protocol defined by Kenwood for the
  TS-2000 transceiver family. The `ZZ…` extended command set used by this
  project is a PowerSDR / Thetis-specific extension layered on top of the
  Kenwood base protocol, publicly documented in the PowerSDR user manual
  and the Thetis change logs; implemented here from that public
  documentation
