#!/bin/sh

set -eu

version="${AGUL_VERSION:-{{VERSION}}}"
install_dir="${AGUL_INSTALL_DIR:-${HOME:-}/.local/bin}"
dry_run=false

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --install-dir) install_dir="$2"; shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        -h|--help)
            echo "Usage: install.sh [--version VERSION] [--install-dir DIRECTORY] [--dry-run]"
            exit 0
            ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$version" in
    *'{{'*'}}'*)
        echo "this checkout installer has no embedded release; pass --version or set AGUL_VERSION" >&2
        exit 2
        ;;
esac
version=${version#v}
[ -n "$install_dir" ] || {
    echo "HOME is unavailable; pass --install-dir" >&2
    exit 2
}

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-gnu ;;
    Darwin/x86_64|Darwin/amd64) target=x86_64-apple-darwin ;;
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    *) echo "unsupported platform: $(uname -s)/$(uname -m)" >&2; exit 2 ;;
esac

archive="agul-v${version}-${target}.tar.gz"
url="https://github.com/storious/agul/releases/download/v${version}/${archive}"
destination="${install_dir}/agul"
use_github_cli=false
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    use_github_cli=true
fi

echo "Agul ${version} -> ${destination}"
if [ "$dry_run" = true ]; then
    if [ "$use_github_cli" = true ]; then
        echo "gh release download v${version} --repo storious/agul --pattern ${archive}"
    else
        echo "$url"
    fi
    exit 0
fi

command -v tar >/dev/null || { echo "tar is required" >&2; exit 2; }

temporary=$(mktemp -d "${TMPDIR:-/tmp}/agul-install.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
if [ "$use_github_cli" = true ]; then
    gh release download "v${version}" \
        --repo storious/agul \
        --pattern "$archive" \
        --dir "$temporary"
else
    command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
    curl -fL "$url" -o "$temporary/$archive"
fi
tar -xzf "$temporary/$archive" -C "$temporary"
mkdir -p "$install_dir"
cp "$temporary/agul-v${version}-${target}/agul" "$destination"
chmod +x "$destination"
echo "Installed ${destination}"
case ":${PATH:-}:" in
    *:"$install_dir":*) ;;
    *) echo "Add ${install_dir} to PATH to run agul from a new shell." ;;
esac
