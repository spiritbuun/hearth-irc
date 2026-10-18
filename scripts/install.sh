#!/bin/sh
# Install built binaries; never starts services or changes runtime configuration.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prefix=/usr/local
destdir=
bins="hearthd hearth-ircd hearth-admin"
if [ -d "$root/bin" ]; then
    build_dir="$root/bin"
else
    build_dir="${CARGO_TARGET_DIR:-$root/target}/release"
fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix|--destdir|--bin-dir)
            [ "$#" -ge 2 ] || { echo "Missing value for $1" >&2; exit 2; }
            case "$1" in
                --prefix) prefix=$2 ;;
                --destdir) destdir=$2 ;;
                --bin-dir) build_dir=$2 ;;
            esac
            shift 2 ;;
        --client-only) bins=hearthd; shift ;;
        -h|--help)
            echo "Usage: sh scripts/install.sh [--prefix /usr/local] [--destdir /staging] [--bin-dir PATH] [--client-only]"
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done
case "$prefix" in /*) ;; *) echo "--prefix must be absolute" >&2; exit 2 ;; esac
case "$destdir" in ''|/*) ;; *) echo "--destdir must be absolute" >&2; exit 2 ;; esac
for binary in $bins; do
    [ -x "$build_dir/$binary" ] || {
        echo "Missing $build_dir/$binary; run cargo build --release --locked --bins first." >&2
        exit 1
    }
    [ ! -d "$destdir$prefix/bin/$binary" ] || {
        echo "Refusing to replace a directory: $destdir$prefix/bin/$binary" >&2
        exit 1
    }
done

bindir="$destdir$prefix/bin"
share="$destdir$prefix/share/hearth"
install -d "$bindir" "$share"
stage=$(mktemp -d "$bindir/.hearth-install.XXXXXX")
trap 'rm -rf "$stage"' EXIT
trap 'exit 1' HUP INT TERM
for binary in $bins; do
    install -m 755 "$build_dir/$binary" "$stage/$binary"
done
for binary in $bins; do
    # Rename allows upgrades while an old executable is still running.
    mv -f "$stage/$binary" "$bindir/$binary"
done
install -m 644 "$root/README.md" "$share/README.md"
for license in "$root"/LICENSE*; do
    [ ! -f "$license" ] || install -m 644 "$license" "$share/"
done
for directory in docs deploy skills; do
    cp -R "$root/$directory" "$share/"
done
echo "Installed $bins in $bindir"
echo "Documentation, service examples, and agent skill: $share"
echo "Add $prefix/bin to PATH if needed. Services are not started automatically."
