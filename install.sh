#!/usr/bin/env bash
# Installs winv-clipboard for the current user. No sudo, no extra permissions.
#
#   curl -fsSL https://raw.githubusercontent.com/ajaysavaliya8/Xcopy/main/install.sh | bash
#
# downloads the prebuilt program from the latest GitHub release and installs it. Run from a clone
# of the repository (./install.sh) it builds the program from source instead, when Rust is present.
# Safe to run again: it upgrades in place. Settings (all optional):
#   WINV_VERSION=v1.0.0        install that release instead of the latest
#   WINV_DOWNLOAD_BASE=<url>   download from here instead of GitHub (mirrors, testing)
set -euo pipefail

# GitHub "owner/name" that publishes the releases.
REPO="ajaysavaliya8/Xcopy"

have() { command -v "$1" >/dev/null 2>&1; }

die() {
    echo "error: $*" >&2
    exit 1
}

fetch() { # fetch <url> <file>
    if have curl; then
        curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"
    elif have wget; then
        wget -q --tries=3 --timeout=15 -O "$2" "$1"
    else
        die "neither curl nor wget is installed. Install one with: sudo apt install curl"
    fi
}

check_desktop() {
    if [ -n "${XDG_CURRENT_DESKTOP:-}" ] && [[ "$XDG_CURRENT_DESKTOP" != *GNOME* ]]; then
        echo "warning: this app targets Ubuntu's GNOME desktop (found: $XDG_CURRENT_DESKTOP)" >&2
    fi
}

build_from_source() { # build_from_source <checkout dir>
    cd "$1"
    echo "Building from source (this takes a few minutes the first time)..."
    cargo build --release --locked
    ./target/release/winv-clipboard install </dev/null
}

download_and_install() {
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "unsupported CPU type: $(uname -m) (supported: x86_64, aarch64)" ;;
    esac

    local version="${WINV_VERSION:-latest}" base
    if [ "$version" = latest ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        base="https://github.com/$REPO/releases/download/$version"
    fi
    # The placeholder is split in two so that replacing it in this file (sed) cannot edit this check.
    if [ -z "${WINV_DOWNLOAD_BASE:-}" ] && [[ "$REPO" == "YOUR-GITHUB""-USER/"* ]]; then
        die "REPO at the top of install.sh has not been set to your GitHub owner/name yet"
    fi
    base="${WINV_DOWNLOAD_BASE:-$base}"

    local asset="winv-clipboard-$arch-linux.tar.gz" tmp
    tmp="$(mktemp -d)"
    # Expanded now, not when the trap fires: `tmp` is local and gone by then.
    trap "rm -rf '$tmp'" EXIT

    echo "Downloading $asset ($version)..."
    fetch "$base/$asset" "$tmp/$asset" || die "could not download $base/$asset (is there a release yet? see https://github.com/$REPO/releases)"
    fetch "$base/$asset.sha256" "$tmp/$asset.sha256" || die "could not download the checksum file $base/$asset.sha256"
    (cd "$tmp" && sha256sum --check --status "$asset.sha256") || die "the download is corrupt (checksum mismatch); nothing was installed"

    tar -xzf "$tmp/$asset" -C "$tmp" winv-clipboard
    "$tmp/winv-clipboard" install </dev/null
}

main() {
    check_desktop

    # The directory this script sits in, if it is run from a clone of the repository. When piped
    # into bash (the curl one-liner) there is no such file, and whatever directory the user happens
    # to be in must never be mistaken for a checkout.
    local dir=""
    if [ -f "$SELF" ] && grep -q '^name = "winv-clipboard"' "$(dirname "$SELF")/Cargo.toml" 2>/dev/null; then
        dir="$(cd "$(dirname "$SELF")" && pwd)"
    fi

    if [ -n "$dir" ]; then
        if [ -x "$HOME/.cargo/bin/cargo" ]; then
            export PATH="$HOME/.cargo/bin:$PATH"
        fi
        if have cargo && have cc; then
            build_from_source "$dir"
            return
        fi
        echo "Rust and a C compiler are not both installed here, so using the prebuilt release instead."
    fi
    download_and_install
}

# Empty when piped into bash. (Inside a function BASH_SOURCE would say "bash", hence read here.)
SELF="${BASH_SOURCE[0]:-}"

# Everything runs from `main`, so a download cut short can never execute half a script.
main "$@"
