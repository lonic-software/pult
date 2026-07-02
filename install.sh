#!/bin/sh
# pult installer — macOS, Linux, and Git Bash on Windows.
#
#   curl -fsSL https://raw.githubusercontent.com/lonic-software/pult/main/install.sh | sh
#
# Environment overrides:
#   PULT_VERSION      install a specific tag, e.g. v0.1.0 (default: latest release)
#   PULT_INSTALL_DIR  where to put the binary            (default: ~/.local/bin)
#   PULT_REPO         GitHub repo slug                   (default: lonic-software/pult)
#   PULT_BASE_URL     full base URL for the assets (mirrors / air-gapped setups);
#                    overrides PULT_REPO/PULT_VERSION entirely
set -eu

REPO="${PULT_REPO:-lonic-software/pult}"
VERSION="${PULT_VERSION:-latest}"
INSTALL_DIR="${PULT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
err() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }

# ── detect platform ─────────────────────────────────────────────
os=$(uname -s)
case "$os" in
    Darwin)               suffix="apple-darwin";      ext="tar.gz" ;;
    Linux)                suffix="unknown-linux-musl"; ext="tar.gz" ;;
    MINGW*|MSYS*|CYGWIN*) suffix="pc-windows-msvc";   ext="zip" ;;
    *) err "unsupported OS: $os" ;;
esac

arch=$(uname -m)
case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64)  arch="x86_64" ;;
    *) err "unsupported architecture: $arch" ;;
esac
if [ "$suffix" = "pc-windows-msvc" ] && [ "$arch" = "aarch64" ]; then
    err "no Windows ARM build yet — build from source: cargo install --path ."
fi

target="${arch}-${suffix}"
asset="pult-${target}.${ext}"
if [ -n "${PULT_BASE_URL:-}" ]; then
    base="$PULT_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
else
    base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

# ── download ────────────────────────────────────────────────────
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        err "need curl or wget"
    fi
}

say "downloading ${base}/${asset}"
fetch "${base}/${asset}" "${tmp}/${asset}" \
    || err "download failed — does a release exist for ${target}?"

# ── verify (best effort: skip only if no sha tool is available) ─
if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then
    if fetch "${base}/checksums.txt" "${tmp}/checksums.txt"; then
        (
            cd "$tmp"
            grep " ${asset}\$" checksums.txt > asset.sum \
                || err "${asset} missing from checksums.txt"
            if command -v sha256sum >/dev/null 2>&1; then
                sha256sum -c asset.sum >/dev/null
            else
                shasum -a 256 -c asset.sum >/dev/null
            fi
        ) || err "checksum verification FAILED — refusing to install"
        say "checksum ok"
    else
        say "warning: could not fetch checksums.txt; skipping verification"
    fi
fi

# ── unpack + install ────────────────────────────────────────────
bin="pult"
case "$ext" in
    tar.gz) tar -xzf "${tmp}/${asset}" -C "$tmp" ;;
    zip)
        bin="pult.exe"
        command -v unzip >/dev/null 2>&1 || err "unzip is required"
        unzip -q "${tmp}/${asset}" -d "$tmp"
        ;;
esac
[ -f "${tmp}/${bin}" ] || err "archive did not contain ${bin}"

mkdir -p "$INSTALL_DIR"
if command -v install >/dev/null 2>&1; then
    install -m 755 "${tmp}/${bin}" "${INSTALL_DIR}/${bin}"
else
    cp "${tmp}/${bin}" "${INSTALL_DIR}/${bin}"
    chmod 755 "${INSTALL_DIR}/${bin}"
fi

say "installed ${INSTALL_DIR}/${bin} — $("${INSTALL_DIR}/${bin}" --version 2>/dev/null || echo 'pult')"

case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        say ""
        say "note: ${INSTALL_DIR} is not on your PATH. Add this to your shell profile:"
        say "    export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac

# Runtime dependencies — pult needs these to run, not to install.
# Warn, don't fail: the user may be installing before setting up the rest.
missing=""
command -v bash >/dev/null 2>&1 || missing="bash"
command -v git >/dev/null 2>&1 || missing="${missing:+$missing and }git"
if [ -n "$missing" ]; then
    say ""
    say "warning: $missing not found on PATH. pult executes commands via bash"
    say "and fetches git modules via git — install $missing before running pult."
    case "$suffix" in
        pc-windows-msvc) say "On Windows, Git for Windows provides both: https://gitforwindows.org" ;;
    esac
fi
