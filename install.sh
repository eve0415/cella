#!/bin/sh
set -eu

REPO="eve0415/cella"

# Platform detection
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)  TARGET_OS="unknown-linux-musl" ;;
  darwin) TARGET_OS="apple-darwin" ;;
  *)
    echo "Error: Unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  TARGET_ARCH="x86_64" ;;
  aarch64|arm64) TARGET_ARCH="aarch64" ;;
  *)
    echo "Error: Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"

# Version: use CELLA_VERSION env var or fetch latest release
if [ -n "${CELLA_VERSION:-}" ]; then
  VERSION="$CELLA_VERSION"
else
  VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)
fi
VERSION_NUM="${VERSION#v}"

ARCHIVE="cella-v${VERSION_NUM}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
CHECKSUM_URL="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading cella ${VERSION} for ${TARGET}..."
curl -fsSL "$URL" -o "$TMPDIR/$ARCHIVE"
curl -fsSL "$CHECKSUM_URL" -o "$TMPDIR/SHA256SUMS"

# Verify checksum
EXPECTED=$(grep "$ARCHIVE" "$TMPDIR/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL=$(sha256sum "$TMPDIR/$ARCHIVE" | cut -d' ' -f1)
else
  ACTUAL=$(shasum -a 256 "$TMPDIR/$ARCHIVE" | cut -d' ' -f1)
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "Error: Checksum verification failed!" >&2
  echo "  Expected: $EXPECTED" >&2
  echo "  Actual:   $ACTUAL" >&2
  exit 1
fi

echo "Checksum verified."

# Extract
tar xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"

# Install
INSTALL_DIR="${CELLA_INSTALL_DIR:-/usr/local/bin}"

if [ -w "$INSTALL_DIR" ]; then
  cp "$TMPDIR/cella" "$INSTALL_DIR/"
  chmod +x "$INSTALL_DIR/cella"
else
  echo "Installing to $INSTALL_DIR (requires sudo)..."
  sudo cp "$TMPDIR/cella" "$INSTALL_DIR/"
  sudo chmod +x "$INSTALL_DIR/cella"
fi

echo "cella ${VERSION} installed to ${INSTALL_DIR}/cella"
