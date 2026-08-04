#!/bin/sh
# Install backcheck.
#
#   curl -fsSL https://raw.githubusercontent.com/VectorInstitute/backcheck/main/install.sh | sh
#
# Installs to ~/.local/bin by default, so no sudo is needed. Override with
# BACKCHECK_INSTALL_DIR, and pin a version with BACKCHECK_VERSION.

set -eu

REPO="VectorInstitute/backcheck"
INSTALL_DIR="${BACKCHECK_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "this installer needs '$1' on your PATH"
}

need uname
need mkdir
need tar

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
else
    die "this installer needs curl or wget"
fi

# ---------------------------------------------------------------- platform

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Darwin)
        case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) die "unsupported macOS architecture: $arch" ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64)
                die "no prebuilt Linux arm64 binary yet. Build from source with:
  cargo install --git https://github.com/$REPO"
                ;;
            *) die "unsupported Linux architecture: $arch" ;;
        esac
        ;;
    MINGW*|MSYS*|CYGWIN*)
        die "on Windows, download backcheck-x86_64-pc-windows-msvc.zip from
  https://github.com/$REPO/releases/latest"
        ;;
    *)
        die "unsupported operating system: $os"
        ;;
esac

# ---------------------------------------------------------------- download

if [ -n "${BACKCHECK_VERSION:-}" ]; then
    url="https://github.com/$REPO/releases/download/$BACKCHECK_VERSION/backcheck-$target.tar.gz"
    say "Installing backcheck $BACKCHECK_VERSION ($target)"
else
    url="https://github.com/$REPO/releases/latest/download/backcheck-$target.tar.gz"
    say "Installing the latest backcheck ($target)"
fi

tmp="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '$tmp'" EXIT INT TERM

fetch "$url" > "$tmp/backcheck.tar.gz" || die "could not download $url"
tar -xzf "$tmp/backcheck.tar.gz" -C "$tmp" || die "could not extract the archive"
[ -f "$tmp/backcheck" ] || die "the archive did not contain a backcheck binary"

mkdir -p "$INSTALL_DIR"
chmod +x "$tmp/backcheck"
mv -f "$tmp/backcheck" "$INSTALL_DIR/backcheck" \
    || die "could not write to $INSTALL_DIR (set BACKCHECK_INSTALL_DIR to somewhere writable)"

installed="$INSTALL_DIR/backcheck"
say "Installed to $installed"
say ""
"$installed" --version

# ---------------------------------------------------------------- PATH

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        say ""
        say "Try it now:"
        say "  backcheck            check your most recent session"
        say "  backcheck install    run it automatically when Claude Code finishes"
        ;;
    *)
        shell_rc="your shell profile"
        case "${SHELL:-}" in
            */zsh)  shell_rc="~/.zshrc" ;;
            */bash) shell_rc="~/.bashrc" ;;
            */fish) shell_rc="~/.config/fish/config.fish" ;;
        esac
        say ""
        say "$INSTALL_DIR is not on your PATH. Add it in $shell_rc:"
        say ""
        say "  export PATH=\"\$PATH:$INSTALL_DIR\""
        say ""
        say "Or run it directly:"
        say "  $installed"
        ;;
esac
