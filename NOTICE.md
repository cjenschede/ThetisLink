# NOTICE

ThetisLink is a Rust application that interoperates with, and for the Thetis
SDR fork maintained in `cjenschede/Thetis` directly continues, the Thetis
software developed within the OpenHPSDR ecosystem. It is distributed under
the GNU General Public License v2.0-or-later (see `LICENSE`).

See `ATTRIBUTION.md` for the full list of contributors to the upstream
Thetis lineage whose work this project builds upon, and for information
about the dual-licensing statement applicable to contributions by Richard
Samphire (MW0LGE).

Contact for commercial licensing: see `LICENSE-COMMERCIAL.md`.

## Third-party components

ThetisLink vendors and statically links the following third-party Rust crate
as source under `vendor/`, in addition to the crates resolved from
[crates.io](https://crates.io/) at build time. Full per-crate license texts
and SPDX identifiers for every transitive dependency are in
`compliance/THIRD-PARTY-LICENSES.html` and `compliance/sbom.spdx.json`.

- **`mcp2221-hal` v0.1.0**: Copyright © 2025 Rob Wells, dual-licensed
  `MIT OR Apache-2.0`. ThetisLink uses this crate under the **MIT** terms
  (the GPL-2.0-or-later license under which ThetisLink itself is
  distributed is compatible with MIT; the Apache-2.0 alternative is not
  used because Apache-2.0's patent-grant clauses are not compatible with
  GPL-2.0-only). Upstream source:
  <https://github.com/robjwells/mcp2221-hal/>. See
  `vendor/mcp2221-hal/LICENSE-MIT` for the full MIT license text.

  **The vendored copy carries local additions.** Five, each marked in the source
  with `# ThetisLink fork addition`: `MCP2221::connect_with_serial`,
  `connect_with_serial_and_vid_pid`, `connect_with_path`, `list_devices` and the
  `Mcp2221Listing` struct they return. They exist so two boards on one bus can
  be told apart (ThetisLink drives up to six tuners, a rotor and a Morse paddle
  from separate boards) and so the server can offer a serial-number picker.
  None of them is in the upstream release. Apart from those five the tree is
  upstream v0.1.0; `git diff` against the upstream tag shows the difference. The
  MIT terms are unchanged and the copyright notice above is the author's.

## The Microsoft C runtime in the Windows programs

The Windows builds of `ThetisLink-Server.exe` and `ThetisLink-Client.exe` link the
Microsoft C runtime statically (the Universal CRT and the Visual C++ runtime, from
the Windows SDK and the Visual C++ build tools), so they start without the Visual
C++ Redistributable. That runtime is Microsoft's, used under the licence terms of
the Windows SDK and the Visual C++ build tools.

ThetisLink is licensed GPL-2.0-or-later. Under section 1 of GPL version 3 such a
runtime is a System Library: the Universal CRT belongs to the operating system,
the Visual C++ runtime to the compiler. It is therefore not part of the
Corresponding Source of ThetisLink, and its source is not offered here. It does
not appear in the cargo-generated `compliance/sbom.spdx.json`, which lists what
cargo resolves.

## libopus in all three programs

The Opus codec is the C library libopus, built from source into the server, the
desktop client and the Android app by the crate `audiopus_sys` (0.2.2, itself ISC),
which carries that source. libopus is Copyright 2001-2011 Xiph.Org, Skype Limited,
Octasic, Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark
Borgerding and Erik de Castro Lopo, under the BSD-3-Clause licence; its full text,
as it comes with the source, is `compliance/licenses/libopus-BSD-3-Clause.txt`,
and it travels with every release package. Like the C runtime above, it does not
appear in `compliance/sbom.spdx.json` or `compliance/THIRD-PARTY-LICENSES.html`:
cargo sees the crate, not the C library inside it.

## The fonts in the server and the desktop client

The Windows programs draw with egui, whose crate `epaint_default_fonts` (0.29.1)
builds four fonts into them. The Rust licence of that crate (in
`compliance/THIRD-PARTY-LICENSES.html`) is not the licence of the fonts:

- **Ubuntu Light**, under the Ubuntu Font Licence 1.0:
  `compliance/licenses/UFL-1.0.txt`;
- **Hack**, Copyright 2018 Source Foundry Authors, under the MIT licence, with
  the Bitstream Vera licence for the glyphs it derives from:
  `compliance/licenses/Hack-Regular.txt`;
- **emoji-icon-font**, Copyright (c) 2014 John Slegers, under the MIT licence:
  `compliance/licenses/emoji-icon-font-MIT.txt`;
- **Noto Emoji**, under the SIL Open Font Licence 1.1, which the generated
  `THIRD-PARTY-LICENSES.html` carries.

Each text is the file that comes with the font in the crate, unchanged.

## The WebView2 loader in the desktop client

The desktop client opens WebSDR and KiwiSDR pages in an embedded Edge WebView2
(`wry`). The crate `webview2-com-sys` (0.38.2, MIT) links Microsoft's static
WebView2 loader into the client, from the WebView2 SDK 1.0.3650.58. That loader is
Copyright (C) Microsoft Corporation, under the licence of the SDK, which allows
redistribution in binary form with its notice: `compliance/licenses/WebView2-SDK.txt`,
the `LICENSE.txt` of the NuGet package `Microsoft.Web.WebView2` 1.0.3650.58,
unchanged. The WebView2 runtime itself is part of Windows and is not shipped.

## HIDAPI in the server and the desktop client

The crate `hidapi` (2.6.6) builds the C library HIDAPI (Alan Ott, Signal 11
Software, and the libusb/hidapi team) into the server and the desktop client,
for the MCP2221A boards. HIDAPI may be used under one of three licences, at the
choice of whoever uses it; ThetisLink uses it under **the original HIDAPI
licence**, which asks that the copyright notice in the source files stays intact.
The text that offers the choice is in `compliance/THIRD-PARTY-LICENSES.html`.

## Oboe in the Android app

The crate `oboe-sys` (0.6.1) builds the C++ library Oboe, Copyright 2015 The
Android Open Source Project, into the app's native library, for low-latency
audio. Oboe is under the Apache License 2.0; its text is
`compliance/licenses/Apache-2.0.txt`, and it travels with every release package.
cargo sees the crate, not the library inside it.

## The Rust standard library

Every program carries parts of the Rust standard library (`std`, `core`,
`alloc`), Copyright The Rust Project Developers, offered under Apache-2.0 or
MIT; ThetisLink uses it under **MIT**, as it does `mcp2221-hal`. The Unicode
tables in `core` are under the Unicode licence (Unicode-3.0). cargo does not
list the standard library; the texts of both licences are in
`compliance/THIRD-PARTY-LICENSES.html`.

## The C++ runtime and libffi in the Android app

The app's native library links the C++ runtime of the Android NDK statically:
libc++, libc++abi, libunwind and the compiler-rt builtins of LLVM, under
Apache-2.0 with the LLVM exception (and, for older parts, the earlier LLVM
licence the NDK ships beside it). The JNA library in the app (`libjnidispatch`,
Apache-2.0, see `ATTRIBUTION.md`) contains libffi, under the MIT licence of its
authors.
