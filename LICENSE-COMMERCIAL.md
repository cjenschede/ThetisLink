# Commercial Licensing

ThetisLink is primarily distributed under the GNU General Public License
v2.0-or-later (see `LICENSE`). The GPL version is freely available and
suitable for the vast majority of use cases, including amateur radio use,
personal use, research, and non-commercial redistribution subject to the
GPL terms.

If your intended use requires licensing terms other than the GPL (for
example, inclusion in proprietary software without GPL obligations,
commercial redistribution under different terms, or any other purpose where
GPL is not a fit) please contact the author:

- **Author:** PA3GHM (Chiron van der Burgt)
- **GitHub:** [cjenschede](https://github.com/cjenschede)

## Scope of what can be commercially licensed

Any commercial-licensing arrangement entered into through this contact path
covers **only the rights that the author (PA3GHM, Chiron van der Burgt)
personally owns** in:

- the ThetisLink Rust workspace (server, desktop client, Android client,
  shared core/logic crates) authored under `cjenschede/ThetisLink` /
  `cjenschede/sdr-remote`;
- the ThetisLink-specific PA3GHM additions to the Thetis fork at
  `cjenschede/Thetis` branch `thetislink-tl2`: that is, the TL2 `_ex`
  TCI extensions, capability broadcast, server-side CTUN auto-recenter,
  diversity auto-null suite, per-RX DDC sample-rate broadcast, and the
  associated `ThetisLink extensions` checkbox gating logic, in the
  precise form in which those additions were contributed by PA3GHM.

A commercial agreement under this document **does not, and cannot**, relicense:

- **Upstream Thetis / FlexRadio PowerSDR / OpenHPSDR / MW0LGE work**:
  the underlying Thetis lineage carries its own copyrights and licence
  terms, including the dual-licensing statement issued by Richard Samphire
  (MW0LGE) in 2026. Anyone seeking commercial use of upstream Thetis must
  approach the relevant upstream copyright holders directly. See
  `ATTRIBUTION.md` for the full lineage and contributor list.
- **Third-party Rust / Kotlin / Java dependencies**: every transitive
  dependency carries its own licence. See `compliance/THIRD-PARTY-LICENSES.html`
  and `compliance/sbom.spdx.json` for the per-crate inventory. Vendored
  dependencies (e.g. `vendor/mcp2221-hal/`) likewise retain their original
  upstream licences: `mcp2221-hal` is dual-licensed `MIT OR Apache-2.0`
  and ThetisLink uses it under the MIT terms; relicensing it commercially
  is between the licensee and the upstream author, not this contact.
- **Other contributors' work**: patches accepted from third parties into
  ThetisLink remain under those contributors' chosen licence terms; PA3GHM
  does not aggregate or relicense their copyrights.

In other words: this contact is the right path to obtain commercial terms
for PA3GHM's own contributions only. The GPL chain on the lineage and on
every dependency stays intact independently.

Commercial licensing terms for PA3GHM's contributions are negotiated on a
case-by-case basis. No rights beyond those granted by the GPL (see `LICENSE`)
are conferred except by a separate, signed written agreement between the
author and the licensee.
