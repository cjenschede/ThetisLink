#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Build the Linux server and client, and check what came out.
#
#   bash build-linux.sh TOOLCHAIN TARGET_DIR OUT_DIR [--release]
#
# The script finds the source tree one folder up from where it sits.
#
#   TOOLCHAIN   the Rust version to build with: the one the releases are built
#               with, named in the installation guide; rustup must have it
#               installed
#   TARGET_DIR  cargo's target directory
#   OUT_DIR     where the two binaries, build-info.txt and the package are put
#   --release   build a release: this machine must itself have the glibc the
#               releases are built against (MAX_GLIBC), and binaries that need a
#               newer one are refused; without it the script only says what they
#               need, so a build on a newer Linux for that same machine is fine
#
# The one place that knows how ThetisLink is built for Linux, on a development
# PC and on a build machine alike, for the architecture of the machine it runs
# on: x86_64 or aarch64 (64-bit ARM, such as a Raspberry Pi with a 64-bit
# system). Two machines that run it on the same commit with the same Rust and
# the same C toolchain packages are meant to produce the same bytes;
# build-info.txt says what went in (the packages: line names their Debian
# revisions), so two builds can be compared.
#
# Needed: rustup with the toolchain, a C compiler, cmake, pkg-config, binutils
# (readelf, objdump), a grep with -P (GNU grep), GNU tar and gzip, and the
# development packages of ALSA and libudev (on Debian and Ubuntu:
# libasound2-dev, libudev-dev).
#
# Before the build it checks, with --release, the glibc of this machine, and
# writes build-info.txt; after the build it checks four things, and stops on
# the first that fails:
#   - both binaries are newer than lib.rs, which every change of the build
#     number touches, so an older binary was not rebuilt;
#   - they ask the system for nothing beyond the libraries a user already has
#     (ALLOWED_NEEDED below): OpenSSL, GTK or WebKitGTK coming back would show
#     up here before it shows up on somebody's machine;
#   - which glibc they need at least, said out loud, because that decides which
#     Linux versions they run on, and with --release no more than MAX_GLIBC;
#   - no path of this machine is left in them: not the home directory, not the
#     source folder, not the target directory (see "path remapping" below).
# Then it checks that the libraries the window loads are the ones the README
# names (WINDOW_LIBS), and packs the two binaries with their licence texts and a
# README into ThetisLink-<version>-linux-<architecture>.tar.gz, the same bytes
# on every machine that built the same binaries (see "the package" at the end).
set -u

RELEASE=0
if [ $# -eq 4 ] && [ "$4" = "--release" ]; then
    RELEASE=1
elif [ $# -ne 3 ]; then
    echo "usage: $0 TOOLCHAIN TARGET_DIR OUT_DIR [--release]" >&2
    exit 2
fi
TOOLCHAIN=$1
TARGET_DIR=$2
OUT_DIR=$3

REPO="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
LIB_RS="$REPO/sdr-remote-core/src/lib.rs"
# The programs of a build: cargo builds these, and the output folder and the
# package take these. The README text further down names them in prose.
BINS="ThetisLink-Server ThetisLink-Client"

# What a Linux binary of ThetisLink may ask the dynamic loader for. libudev
# (serial ports, HID) and ALSA (audio) are the two a desktop Linux has as a
# matter of course; the rest is the C runtime, and the dynamic loader of the
# architecture (LOADER, below). TLS is rustls, built in, and the WebSDR window
# is Windows only, so neither OpenSSL nor GTK belongs here. The README in the
# package names the system libraries a user may have to install; the C runtime
# comes with every Linux and is not named there.
SYSTEM_LIBS="libudev.so.1 libasound.so.2"
C_RUNTIME="libgcc_s.so.1 libm.so.6 libc.so.6"
ALLOWED_NEEDED="$SYSTEM_LIBS $C_RUNTIME"

# The libraries the window loads itself at run time (winit and glutin open X11
# or Wayland, xkbcommon and EGL/GL with dlopen, so readelf does not see them).
# The README names them. The list is written here, not derived: a search of the
# bytes can miss a name or make one up. That search still runs after the build,
# as a guard: a library name in the binaries that is in neither this list, nor
# NAMED_NOT_LOADED, nor the ones the loader loads, or a name in this list that
# is no longer in the binaries, stops the build until the lists are brought up
# to date. What the guard
# cannot see is a name it does not recognise as one (a dot in the stem, such as
# libglib-2.0.so.0).
WINDOW_LIBS="libEGL.so.1 libGL.so.1 libX11-xcb.so.1 libX11.so.6 libXcursor.so.1 libXi.so.6
libXrender.so.1 libwayland-client.so.0 libwayland-egl.so.1 libxcb.so.1 libxkbcommon-x11.so.0
libxkbcommon.so.0"
# Library names that may be in the bytes but are never loaded, each with its
# reason; the README leaves them out, and the guard accepts them on either
# architecture. libXinerama.so.1: x11-dl carries the name of its Xinerama part,
# which no crate in the tree calls; the aarch64 binaries keep the name, the
# x86_64 ones do not.
NAMED_NOT_LOADED="libXinerama.so.1"

# The highest glibc a release binary may ask for (with --release): the glibc of
# Ubuntu 22.04, on which the releases are built. Which systems that covers is
# named per architecture, in SYSTEMS below (the README carries it). A build on
# a newer system produces binaries that need more, and those are refused here,
# not found on a user's machine. With --release the glibc of this machine must
# be exactly this one, so this value, the Ubuntu version the setup of the build
# machine insists on and the image it starts from cannot drift apart unseen.
MAX_GLIBC="2.35"

# The package: what goes in beside the two binaries, from the source tree. The
# licence texts are those of the desktop programs (the two Windows exe's carry
# the same list): ThetisLink's own licence and notices, the list of every crate
# with its licence text (it covers the crates of every platform), the SBOM, and
# the texts of what is built in without a crate of its own (libopus, the fonts
# of egui). The WebView2 text is not here: that is the Windows client only.
# Every file must also be in the public source tree, which builds the same
# package.
PACKAGE_FILES="LICENSE NOTICE.md ATTRIBUTION.md
compliance/THIRD-PARTY-LICENSES.html compliance/sbom.spdx.json
compliance/licenses/libopus-BSD-3-Clause.txt compliance/licenses/UFL-1.0.txt
compliance/licenses/Hack-Regular.txt compliance/licenses/emoji-icon-font-MIT.txt"
# One time for every file in the package, so a package does not change with
# the day it was made or the clock of the machine: 2020-01-01 00:00 UTC. Not
# the commit time: the public tree that builds the same package has no git.
PACKAGE_MTIME="@1577836800"
# Where the source code of what is in the package is published (GPL-2.0 asks
# that a binary says where its source is).
SOURCE_URL="https://github.com/cjenschede/ThetisLink"

ok() { echo "  [OK]   $*"; }
die() { echo "  [FAIL] $*"; exit 1; }

# Asking rustup for a toolchain it lacks makes it download and install one. A
# check must not change the machine, and a build must not pick up a toolchain
# nobody chose.
export RUSTUP_AUTO_INSTALL=0

# A RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS in the environment replaces the
# target rustflags outright, and the path remapping below lives there: the build
# would carry the paths of this machine again. The Windows release build clears
# both for the same reason. Some CI setups set RUSTFLAGS by default. The
# per-target variables are cleared too, every CARGO_TARGET_*_RUSTFLAGS in the
# environment and not a list per architecture (the triple is not known yet
# here), so the rustflags are only what this script passes. CARGO_INCREMENTAL=1
# gives other bytes too (measured), and no line of build-info.txt would show why.
# compgen -e lists only names a shell can hold; one with a dash cargo does not
# read either. The Windows release build clears the same names.
for v in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS \
         $(compgen -e | grep -E '^CARGO_TARGET_[A-Z0-9_]+_RUSTFLAGS$') CARGO_INCREMENTAL; do
    if [ -n "${!v+x}" ]; then
        echo "  [INFO] $v was set (${!v}); cleared, the build passes its own"
        unset "$v"
    fi
done

# libopus: always the copy that comes with audiopus_sys, built through CMake.
# With libopus-dev installed audiopus_sys would take the system's instead, and
# two machines would build two different binaries from the same commit.
# OPUS_NO_PKG_CONFIG works through the pkg-config crate (NAME_NO_PKG_CONFIG),
# not through audiopus_sys itself; after a failed probe audiopus_sys reads
# LIBOPUS_LIB_DIR and OPUS_LIB_DIR, so those are cleared below.
export OPUS_NO_PKG_CONFIG=1

# Variables that would undo the pkg-config fence further down, point
# audiopus_sys at a libopus on the machine, or add C compiler flags: the cc crate
# adds CFLAGS_<target>, TARGET_CFLAGS and the like to the CFLAGS this script
# sets, CRATE_CC_NO_DEFAULTS drops its own default flags, CC_<target> and its
# kin pick another compiler, every CMAKE_* and its HOST_/TARGET_ forms (the
# cmake crate reads each of its variables as _<target>, HOST_, TARGET_ and bare,
# so CMAKE_<target> picks another cmake program) changes how libopus is
# configured, and OPUS_STATIC or LIBOPUS_STATIC changes how it is linked: each
# gives other bytes. The bare CC, CXX and CMAKE stay a deliberate choice, and
# build-info.txt names them. CMAKE_POLICY_VERSION_MINIMUM stays too: the opus
# sources ask for cmake 3.1, which CMake 4 no longer accepts without it, so on a
# machine with CMake 4 it decides whether libopus configures at all. The
# releases are built with CMake 3, where it does nothing. The pkg-config crate reads the
# target-specific forms before the bare ones, and one of them carries the
# target triple with dashes (PKG_CONFIG_PATH_x86_64-unknown-linux-gnu): bash
# cannot unset a name like that and does not even list it, but hands it on to
# cargo. So the names come from env itself; what bash can unset it unsets, the
# rest is taken out of cargo's environment with env -u. gcc itself reads CPATH,
# C_INCLUDE_PATH and LIBRARY_PATH: another header or library folder for libopus
# and hidapi, so they go as well.
NOT_FOR_CARGO=()
while IFS= read -r -d '' pair; do
    v="${pair%%=*}"
    case "$v" in
        CMAKE_POLICY_VERSION_MINIMUM) continue ;;
        PKG_CONFIG*|HOST_PKG_CONFIG*|TARGET_PKG_CONFIG*|LIBOPUS_LIB_DIR|OPUS_LIB_DIR) ;;
        CFLAGS_*|CXXFLAGS*|TARGET_CFLAGS|HOST_CFLAGS|TARGET_CXXFLAGS|HOST_CXXFLAGS) ;;
        CRATE_CC_NO_DEFAULTS|CC_*|CXX_*|TARGET_CC|HOST_CC|TARGET_CXX|HOST_CXX|CMAKE_TOOLCHAIN_FILE*) ;;
        CMAKE_*|HOST_CMAKE*|TARGET_CMAKE*|LIBOPUS_STATIC|OPUS_STATIC) ;;
        CPATH|C_INCLUDE_PATH|CPLUS_INCLUDE_PATH|LIBRARY_PATH) ;;
        *) continue ;;
    esac
    echo "  [INFO] $v was set; cleared, the build sees only what this script allows"
    case "$v" in
        *[!A-Za-z0-9_]*) NOT_FOR_CARGO+=(-u "$v") ;;
        *) unset "$v" ;;
    esac
done < <(env -0)

# pkg-config sees only the system libraries the build needs: ALSA and libudev,
# the same two that ALLOWED_NEEDED lets the binaries ask for. Without this the
# result depends on which other development packages happen to be on the
# machine: x11-dl writes the libdir of every X11 library pkg-config can find
# into the binary, so a machine with the X11 development packages built other
# bytes than one without. x11-dl tries the plain library name first either way,
# so leaving the path out changes nothing for a user.
PKG_CONFIG_ALLOWED="alsa libudev"

CARGO="${CARGO_BIN:-${CARGO_HOME:-$HOME/.cargo}/bin/cargo}"
RUSTC="${RUSTC_BIN:-${CARGO_HOME:-$HOME/.cargo}/bin/rustc}"
[ -x "$CARGO" ] || die "cargo not found at $CARGO"

have="$("$RUSTC" "+$TOOLCHAIN" --version 2>&1)" || die "Rust $TOOLCHAIN is not installed here: $have"
case "$have" in
    "rustc $TOOLCHAIN "*) ok "$have" ;;
    *) die "asked for Rust $TOOLCHAIN, got: $have" ;;
esac

# The architecture: the target is the machine itself, as rustc sees it. What
# differs per architecture is named here and nowhere else. SYSTEMS are the ones
# with glibc MAX_GLIBC or newer that exist for this architecture: Linux Mint is
# x86_64 only, Raspberry Pi OS is ARM only.
TRIPLE="$("$RUSTC" "+$TOOLCHAIN" -vV | sed -n 's/^host: //p')"
case "$TRIPLE" in
    x86_64-unknown-linux-gnu) ARCH=x86_64; LOADER=ld-linux-x86-64.so.2
        SYSTEMS="Ubuntu 22.04, Debian 12, Linux Mint 21, or newer" ;;
    aarch64-unknown-linux-gnu) ARCH=aarch64; LOADER=ld-linux-aarch64.so.1
        SYSTEMS="Ubuntu 22.04, Debian 12, Raspberry Pi OS 12 64-bit, or newer" ;;
    *) die "this script builds for x86_64 or aarch64 Linux; rustc here builds for '${TRIPLE:-unknown}'" ;;
esac
ALLOWED_NEEDED="$ALLOWED_NEEDED $LOADER"
ok "building for $TRIPLE"

machine_glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null)"; machine_glibc="${machine_glibc#glibc }"
if [ "$RELEASE" = 1 ]; then
    [ "$machine_glibc" = "$MAX_GLIBC" ] ||
        die "this machine has glibc ${machine_glibc:-unknown}; a release is built where it is $MAX_GLIBC (Ubuntu 22.04)"
    ok "glibc of this machine: $machine_glibc, as a release asks"
fi

mkdir -p "$TARGET_DIR" "$OUT_DIR" || die "cannot create $TARGET_DIR or $OUT_DIR"
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"
# What an earlier build left in the output folder goes first: a failed build
# must not leave a fresh build-info.txt beside old binaries.
rm -f "$OUT_DIR/build-info.txt" "$OUT_DIR"/ThetisLink-*-linux-*.tar.gz || die "cannot clear $OUT_DIR"
for b in $BINS; do rm -f "$OUT_DIR/$b" || die "cannot clear $OUT_DIR"; done

PC_DIR="$TARGET_DIR/pkgconfig-allowed"
rm -rf "$PC_DIR" && mkdir -p "$PC_DIR" || die "cannot create $PC_DIR"
for pc in $PKG_CONFIG_ALLOWED; do
    from="$(pkg-config --variable=pcfiledir "$pc" 2>/dev/null)" || from=""
    [ -n "$from" ] && [ -f "$from/$pc.pc" ] || die "pkg-config does not know $pc: install its development package"
    cp "$from/$pc.pc" "$PC_DIR/" || die "cannot copy $pc.pc"
done
export PKG_CONFIG_LIBDIR="$PC_DIR"

# Path remapping, as the Windows release build does. Without it the binary
# carries the home directory (the folder name is often the person's name) and
# the target directory: generated code such as the glutin bindings is compiled
# from there, so two builds in two directories differed in every byte after the
# first path. rustc and gcc both apply the LAST matching prefix, so the wider
# folders come first: the home directory, then what may sit inside it, and the
# target directory, which may sit inside the source folder, last. rustc's flags
# cover Rust; -ffile-prefix-map does the same for the C sources (libopus through
# CMake, hidapi).
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
SYSROOT="$("$RUSTC" "+$TOOLCHAIN" --print sysroot)" || die "rustc --print sysroot failed"
for p in "$CARGO_HOME_DIR" "$SYSROOT" "$REPO" "$TARGET_DIR" "$HOME"; do
    case "$p" in *"'"*|*" "*) die "a path with a quote or a space cannot be remapped here: $p" ;; esac
done
# The source and target folders are searched for in the binaries as plain text
# after the build. A folder of one level (/src, /target) is also a piece of
# other paths in them (.../src/lib.rs), so that search would fail on paths that
# are not of this machine; in a container, mount them a level deeper.
for p in "$REPO" "$TARGET_DIR"; do
    case "$p" in /*/*) ;; *) die "$p is too short to be told apart from other paths in the binaries: use a folder at least two levels deep" ;; esac
done
REMAP_CONFIG="target.$TRIPLE.rustflags=['--remap-path-prefix=$CARGO_HOME_DIR=/cargo', '--remap-path-prefix=$SYSROOT=/rustc', '--remap-path-prefix=$REPO=/build', '--remap-path-prefix=$TARGET_DIR=/target']"
export CFLAGS="-ffile-prefix-map=$HOME=/home -ffile-prefix-map=$CARGO_HOME_DIR=/cargo -ffile-prefix-map=$REPO=/build -ffile-prefix-map=$TARGET_DIR=/target"

# What is built, and with what: a build on another machine is only comparable
# with the same source, the same Rust and the same C tools (libopus and hidapi
# are C). Written to build-info.txt beside the binaries, with the home directory
# shown as ~ and the source folder as <source>, so the file can travel.
first_line() { local x; x="$("$@" 2>/dev/null | head -1)"; [ -n "$x" ] || x=none; echo "$x"; }
GIT() { git -c safe.directory="$REPO" -C "$REPO" "$@"; }
commit="$(GIT rev-parse HEAD 2>/dev/null)" || commit=""
if [ -z "$commit" ]; then
    commit="unknown (no git here); Cargo.lock sha256 $(sha256sum "$REPO/Cargo.lock" | cut -c1-64)"
elif [ -n "$(GIT status --porcelain --untracked-files=no -- . ':(exclude)docs' ':(exclude)*.md' 2>/dev/null)" ]; then
    commit="$commit (with uncommitted changes outside the documentation)"
# The package carries NOTICE.md and ATTRIBUTION.md: documentation, but a change
# in them changes the package, so they count.
elif [ -n "$(GIT status --porcelain --untracked-files=no -- $PACKAGE_FILES 2>/dev/null)" ]; then
    commit="$commit (with uncommitted changes in a file the package carries)"
fi
os="$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")"; [ -n "$os" ] || os=unknown
INFO="$OUT_DIR/build-info.txt"
tilde="~"
src="<source>"
{
    echo "source: $commit"
    echo "os: $os"
    echo "architecture: $TRIPLE"
    # The page size of the kernel: 4k on x86_64; on aarch64 it depends on the
    # kernel (a Raspberry Pi 5 runs with 16k). Named so a difference between two
    # builds has somewhere to show up.
    echo "page size: $(getconf PAGESIZE 2>/dev/null || echo unknown)"
    echo "glibc: $(first_line ldd --version)"
    if [ "$RELEASE" = 1 ]; then echo "glibc allowed: at most $MAX_GLIBC (--release)"; else echo "glibc allowed: any (no --release)"; fi
    echo "rustc: $("$RUSTC" "+$TOOLCHAIN" --version)"
    echo "cargo: $("$CARGO" "+$TOOLCHAIN" --version)"
    # The C compiler the cc crate uses: CC when it is set (it may carry
    # arguments, so it is split as the cc crate splits it), cc otherwise.
    echo "cc: $(first_line ${CC:-cc} --version)"
    # The Rust binaries are linked by the LLD that comes with rustc (their
    # .comment section says so), so the linker is pinned with the toolchain.
    # binutils still takes part: as and ar build the C sources (libopus, hidapi).
    echo "as: $(first_line as --version)"
    echo "cmake: $(first_line "${CMAKE:-cmake}" --version)"
    # The version lines above can stay the same while the Debian revision of a
    # package changes (a binutils or libc6-dev update), and that can change the
    # bytes. The packages of the C compiler, as, cmake, pkg-config, tar and gzip
    # (the last two make the package) are looked up; the rest are named: the C
    # library and kernel headers that are compiled and linked in, the two system
    # libraries, and the libraries gcc itself runs on (they come from other
    # source packages than gcc; ldd of cc1 lists them). cpp-11 and libgcc-11-dev
    # are bound to the exact version of gcc-11 and need no line of their own. A
    # package dpkg does not know is shown with a ?, and so is a tool no package
    # owns (by its name), not left out. On a machine without dpkg the line says so.
    if command -v dpkg-query >/dev/null 2>&1; then
        # The version of one package, or ? when dpkg has none; the first line
        # that is not empty, so one line per package even when dpkg knows it
        # for two architectures. A CC of only spaces counts as cc.
        pkg_version() { local v; v="$(dpkg-query -W -f='${Version}\n' "$1" 2>/dev/null | grep -m1 .)"; echo "${v:-?}"; }
        read -r -a cc_words <<< "${CC:-cc}"
        list=""
        for t in "${cc_words[0]:-cc}" as "${CMAKE:-cmake}" pkg-config tar gzip; do
            f="$(command -v "$t")"; [ -n "$f" ] && f="$(readlink -f "$f")"
            # With a merged /usr, dpkg knows /bin/tar as /bin/tar, not /usr/bin/tar.
            p=""
            for q in "$f" "${f#/usr}"; do
                [ -n "$q" ] && [ -z "$p" ] && { p="$(dpkg-query -S "$q" 2>/dev/null | head -1)"; p="${p%%:*}"; }
            done
            [ -n "$p" ] || { list="$list$(basename "$t") ?  "; continue; }
            case " $list" in *" $p "*) continue ;; esac
            list="$list$p $(pkg_version "$p")  "
        done
        for p in libc6-dev linux-libc-dev libasound2-dev libudev-dev libmpfr6 libmpc3 libisl23 libgmp10 libzstd1 zlib1g; do
            list="$list$p $(pkg_version "$p")  "
        done
        echo "packages: $list"
    else
        echo "packages: no dpkg on this machine"
    fi
    shown="tool choices: CC: ${CC-unset}  CXX: ${CXX-unset}  CMAKE: ${CMAKE-unset}"; shown="${shown//$REPO/$src}"; echo "${shown//$HOME/$tilde}"
    # Cargo configuration outside the command line also shapes the build (a
    # [profile.release] in a config file, a CARGO_PROFILE_* variable); named
    # here so a difference between two machines has somewhere to show up.
    cfgs=""
    d="$REPO"
    while :; do
        for f in "$d/.cargo/config" "$d/.cargo/config.toml"; do [ -f "$f" ] && cfgs="$cfgs $f"; done
        [ "$d" = / ] && break
        d="$(dirname "$d")"
    done
    for f in "$CARGO_HOME_DIR/config" "$CARGO_HOME_DIR/config.toml"; do [ -f "$f" ] && cfgs="$cfgs $f"; done
    shown="cargo config files:${cfgs:- none}"; shown="${shown//$REPO/$src}"; echo "${shown//$HOME/$tilde}"
    profiles="$(compgen -e | grep '^CARGO_PROFILE_' | paste -sd ' ')"
    echo "cargo profile variables: ${profiles:-none}"
    echo "pkg-config sees: $(cd "$PC_DIR" && for f in *.pc; do printf '%s %s  ' "${f%.pc}" "$(pkg-config --modversion "${f%.pc}")"; done)"
    shown="${CFLAGS//$REPO/$src}"; echo "CFLAGS: ${shown//$HOME/$tilde}"
    shown="${REMAP_CONFIG//$REPO/$src}"; echo "rustflags: ${shown//$HOME/$tilde}"
} > "$INFO" || die "cannot write $INFO"
while IFS= read -r line; do ok "$line"; done < "$INFO"

# Without a controlling terminal: rustc reads the terminal's width itself and
# has crashed on a very narrow one while drawing a warning. setsid -w waits and
# hands back cargo's exit code. env -u takes out what bash could not unset.
echo "Building server and client for Linux (Rust $TOOLCHAIN, --locked)..."
cd "$REPO" || die "cannot enter $REPO"
BIN_ARGS=()
for b in $BINS; do BIN_ARGS+=(--bin "$b"); done
env "${NOT_FOR_CARGO[@]}" CARGO_TARGET_DIR="$TARGET_DIR" setsid -w "$CARGO" "+$TOOLCHAIN" build --release --locked \
    --config "$REMAP_CONFIG" --message-format=short "${BIN_ARGS[@]}" </dev/null
code=$?
[ "$code" -eq 0 ] || die "cargo build returned $code"
ok "cargo finished"

for b in $BINS; do
    f="$TARGET_DIR/release/$b"
    [ -f "$f" ] || die "$b was not built ($f)"
    [ "$f" -nt "$LIB_RS" ] || die "$b is not newer than lib.rs: it is not the build in the tree"
    ok "$b is newer than lib.rs"

    needed="$(readelf -d "$f" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')"
    [ -n "$needed" ] || die "readelf found no NEEDED entries in $b"
    for lib in $needed; do
        case " $ALLOWED_NEEDED " in
            *" $lib "*) ;;
            *) die "$b needs $lib, which is not in ALLOWED_NEEDED. Find who brings it in (cargo tree --target $TRIPLE -i <crate>)" ;;
        esac
    done
    ok "$b needs only: $(echo $needed)"

    glibc="$(objdump -T "$f" | grep -o 'GLIBC_[0-9][0-9.]*' | sort -u -V | tail -1)"
    [ -n "$glibc" ] || die "could not read the glibc versions of $b"
    highest="$(printf '%s\n%s\n' "${glibc#GLIBC_}" "$MAX_GLIBC" | sort -V | tail -1)"
    if [ "$RELEASE" = 1 ]; then
        [ "$highest" = "$MAX_GLIBC" ] || die "$b needs $glibc, more than glibc $MAX_GLIBC: a release is built on Ubuntu 22.04"
        ok "$b needs at least $glibc (a release allows at most $MAX_GLIBC)"
    else
        ok "$b needs at least $glibc"
    fi
    echo "$b needs at least: $glibc" >> "$INFO"
    all_needed="${all_needed:-} $needed"
    all_glibc="${all_glibc:-} ${glibc#GLIBC_}"

    for p in "$REPO" "$TARGET_DIR" "$CARGO_HOME_DIR" "$SYSROOT"; do
        n="$(grep -a -c -F "$p" "$f")"
        [ "$n" -eq 0 ] || die "$b still contains $p ($n lines): the path remapping missed something"
    done
    # The home directory only as a whole name: not after a letter, and not
    # followed by one (/rooted is not /root). Built as
    # root it is /root, and the client carries the AT-SPI object path
    # /org/a11y/atspi/accessible/root, which is not a path of this machine. What
    # lives under the home directory and is known by name (cargo, the sysroot of
    # this toolchain, and the target directory when it sits there) is checked in
    # full above, without this exception.
    n="$(grep -a -c -P "(?<![A-Za-z0-9_.-])\Q$HOME\E(?![A-Za-z0-9_.-])" "$f")"
    case "$n" in ''|*[!0-9]*) die "could not check $b for $HOME: this grep has no -P (PCRE)" ;; esac
    [ "$n" -eq 0 ] || die "$b still contains $HOME ($n lines): the path remapping missed something"
    ok "$b contains no path of this machine"

    cp -p "$f" "$OUT_DIR/$b" || die "cannot copy $b to $OUT_DIR"
done

# The package. The same binaries and the same source files give the same bytes
# on every machine: names in a fixed order, one time for every file
# (PACKAGE_MTIME), owner 0, fixed modes, and gzip without a name or time in its
# header. The text files are taken with LF line ends: git keeps them so, but a
# checkout on Windows read through WSL can hold CRLF, and a package made there
# would then differ from one made from a Linux checkout. The README is written
# here, so the libraries and the glibc it names are the ones these binaries ask
# for.
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || die "could not read the version from Cargo.toml"
NAME="ThetisLink-$VERSION-linux-$ARCH"
STAGE="$TARGET_DIR/package"
rm -rf "$STAGE" && mkdir -p "$STAGE/$NAME" || die "cannot create $STAGE"
for b in $BINS; do
    cp "$OUT_DIR/$b" "$STAGE/$NAME/$b" && chmod 0755 "$STAGE/$NAME/$b" || die "cannot stage $b"
done
for p in $PACKAGE_FILES; do
    [ -f "$REPO/$p" ] || die "$p is missing from the source tree, and the package carries it"
    mkdir -p "$STAGE/$NAME/$(dirname "$p")" &&
        sed 's/\r$//' "$REPO/$p" > "$STAGE/$NAME/$p" && chmod 0644 "$STAGE/$NAME/$p" || die "cannot stage $p"
done
# The system libraries the loader asks for (the C runtime and the loader left
# out), and the ones the window loads at run time (WINDOW_LIBS). readelf gives
# one name per line; one line with spaces for the matching below.
needed_flat="$(echo $all_needed)"
libs="$(for l in $all_needed; do case " $C_RUNTIME $LOADER " in *" $l "*) ;; *) echo "$l" ;; esac; done | LC_ALL=C sort -u | paste -sd ' ')"
# The guard on WINDOW_LIBS: every library name in the bytes of the binaries that
# the loader does not load. A symbol name can run straight into a library name
# (a block of symbols ending in libXi.so.6), so of each hit the last lib... is
# kept.
window_flat="$(echo $WINDOW_LIBS)"
seen="$(for b in $BINS; do grep -a -o -E 'lib[A-Za-z0-9_+-]*\.so\.[0-9]+' "$OUT_DIR/$b" | sed 's/.*\(lib[A-Za-z0-9_+-]*\.so\.[0-9]*\)$/\1/'; done |
    while read -r l; do case " $needed_flat " in *" $l "*) ;; *) echo "$l" ;; esac; done | LC_ALL=C sort -u | paste -sd ' ')"
for l in $seen; do
    case " $window_flat $(echo $NAMED_NOT_LOADED) " in *" $l "*) ;; *) die "the binaries name $l, which is in neither WINDOW_LIBS nor NAMED_NOT_LOADED: add it to the first if the window loads it (the README names it), to the second with its reason if not" ;; esac
done
for l in $window_flat; do
    case " $seen " in *" $l "*) ;; *) die "$l is in WINDOW_LIBS but no longer in the binaries: take it out of the list" ;; esac
done
ok "the binaries name the libraries of WINDOW_LIBS, and no other than NAMED_NOT_LOADED"
opened="$(for l in $window_flat; do echo "$l"; done | LC_ALL=C sort -u | paste -sd ' ')"
glibc_min="$(for g in $all_glibc; do echo "$g"; done | sort -V | tail -1)"
# Which systems have that glibc: only for a release, built where MAX_GLIBC holds.
systems=""
if [ "$RELEASE" = 1 ]; then
    systems="
($SYSTEMS)"
fi
# The source of exactly this version: a released version (only digits and dots,
# such as 2.14.0) is tagged v<version> in the public repository; a development
# version (2.14.1-dev) is not, so it names the repository. Not on --release:
# development builds are made with it too.
source_at="$SOURCE_URL"
case "$VERSION" in *[!0-9.]*) ;; *) source_at="$SOURCE_URL/tree/v$VERSION" ;; esac
opened_lines="$(echo "$opened" | fmt -w 76)"
cat > "$STAGE/$NAME/README.txt" <<EOF || die "cannot write README.txt"
ThetisLink $VERSION for Linux ($ARCH)

ThetisLink-Server runs beside the SDR software and the radios; ThetisLink-Client
is the desktop client. Unpack the package somewhere you can write to: each
program keeps its settings in a file next to itself.

    ./ThetisLink-Server
    ./ThetisLink-Client

The client opens a window, so it needs a graphical session (X11 or Wayland).
So does the server when it is started without options: then it opens its
settings window, where it is set up, so its first start needs a screen. With
options (for example --tci <host:port>) the server runs without a window.

What the programs need from the system: glibc $glibc_min or newer$systems,
the libraries $libs,
and for a window, loaded at run time (those of the session that runs):
$opened_lines.
A desktop Linux has all of them.

Serial ports (radios, amplifiers, rotators) are only open to members of the
group dialout:

    sudo usermod -aG dialout \$USER

then log out and in again. USB HID devices such as an MCP2221 need read and
write access to their /dev/hidraw device.

ThetisLink is free software under the GNU General Public License, version 2 or
later (LICENSE). The source code: $source_at
NOTICE.md and ATTRIBUTION.md name what is built into these programs and under
which licences; compliance/ holds the full list with the licence texts, and the
SBOM.
EOF
chmod 0644 "$STAGE/$NAME/README.txt" || die "cannot set the mode of README.txt"
find "$STAGE/$NAME" -type d -exec chmod 0755 {} + || die "cannot set the modes in $STAGE"
PKG="$OUT_DIR/$NAME.tar.gz"
tar --sort=name --mtime="$PACKAGE_MTIME" --owner=0 --group=0 --numeric-owner --format=gnu \
    -C "$STAGE" -cf - "$NAME" | gzip -n -9 > "$PKG"
codes="${PIPESTATUS[*]}"
[ "$codes" = "0 0" ] || { rm -f "$PKG"; die "tar or gzip failed (exit codes $codes)"; }
ok "packed $NAME.tar.gz ($(tar -tzf "$PKG" | grep -vc '/$') files)"

(cd "$OUT_DIR" && sha256sum $BINS "$NAME.tar.gz") >> "$INFO"
echo
echo "LINUX BUILD OK"
(cd "$OUT_DIR" && sha256sum $BINS "$NAME.tar.gz")
