#!/bin/sh
# Installs the latest mcpgw release binary into ~/.local/bin (override with
# MCPGW_INSTALL_DIR). Uploaded to every release as mcpgw-installer.sh, so
# piping it from a release URL always matches that release's layout.
set -eu

REPO=kennywillbe/mcpgw
INSTALL_DIR=${MCPGW_INSTALL_DIR:-$HOME/.local/bin}
VERSION=${MCPGW_VERSION:-latest}

fail() {
    echo "mcpgw install: $1" >&2
    exit 1
}

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    *) fail "unsupported OS $(uname -s) — grab a binary from https://github.com/$REPO/releases" ;;
esac

case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "unsupported architecture $(uname -m)" ;;
esac

target="$arch-$os"

if [ "$VERSION" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
    # The archive name carries the version, which the "latest" URL hides, so
    # ask the API for the tag rather than guessing.
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name" *: *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n1)
    [ -n "$VERSION" ] || fail "could not determine the latest version"
else
    base="https://github.com/$REPO/releases/download/v${VERSION#v}"
    VERSION=${VERSION#v}
fi

archive="mcpgw-$VERSION-$target.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $archive"
curl -fsSL "$base/$archive" -o "$tmp/$archive" || fail "download failed: $base/$archive"
tar -xzf "$tmp/$archive" -C "$tmp"

mkdir -p "$INSTALL_DIR"
mv "$tmp/mcpgw-$VERSION-$target/mcpgw" "$INSTALL_DIR/mcpgw"
chmod +x "$INSTALL_DIR/mcpgw"

echo "installed mcpgw $VERSION to $INSTALL_DIR/mcpgw"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "note: $INSTALL_DIR is not on your PATH" ;;
esac
