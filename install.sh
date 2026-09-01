#!/usr/bin/env bash
# Install the latest ana release:
#   curl -fsSL https://raw.githubusercontent.com/intentionally-left-nil/ana/main/install.sh | bash
# Install the enterprise build instead:
#   curl -fsSL https://raw.githubusercontent.com/intentionally-left-nil/ana/main/install.sh | bash -s -- enterprise
set -euo pipefail

REPO="intentionally-left-nil/ana"
INSTALL_DIR="${ANA_INSTALL_DIR:-$HOME/.local/bin}"

case "${1:-community}" in
  community)  pkg="ana" ;;
  enterprise) pkg="ana-enterprise" ;;
  *)          echo "usage: install.sh [community|enterprise]" >&2; exit 2 ;;
esac

case "$(uname -s)" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      echo "ana: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64)        arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "ana: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

target="${arch}-${os}"

tag="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
tag="${tag##*/}"
if [ -z "$tag" ] || [ "$tag" = "latest" ]; then
  echo "ana: could not determine latest release tag" >&2
  exit 1
fi

name="${pkg}-${tag}-${target}"
url="https://github.com/${REPO}/releases/download/${tag}/${name}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "ana: downloading ${url}"
curl -fsSL "$url" -o "${tmp}/${name}.tar.gz"
tar -xzf "${tmp}/${name}.tar.gz" -C "$tmp"

bin="${tmp}/${name}/${pkg}"
if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
fi

mkdir -p "$INSTALL_DIR"
cp "$bin" "${INSTALL_DIR}/${pkg}"
chmod +x "${INSTALL_DIR}/${pkg}"

echo "ana: installed ${tag} (${target}) to ${INSTALL_DIR}/${pkg}"
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "ana: note: ${INSTALL_DIR} is not on your PATH" >&2 ;;
esac
