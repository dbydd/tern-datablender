#!/usr/bin/env bash
# Build the native NetCDF plugin for one target triple and stage a
# distributable that Tern can install as a plain plugin directory.
set -euo pipefail

package_name=tern-datablender
bin_name=tern-datablender

die() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s [target-triple] [--jobs N]\n' "${0##*/}" >&2
    printf '  target-triple  Rust target triple; default is the host triple from rustc -vV\n' >&2
    printf '  --jobs N       cargo parallelism (also read from $JOBS); default 4\n' >&2
    exit 2
}

target=
jobs=${JOBS:-4}
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage ;;
        -t|--target)
            [ $# -ge 2 ] || usage
            [ -z "$target" ] || die "only one target triple may be given"
            target=$2
            shift 2
            ;;
        --target=*)
            [ -z "$target" ] || die "only one target triple may be given"
            target=${1#--target=}
            shift
            ;;
        -j|--jobs)
            [ $# -ge 2 ] || usage
            jobs=$2
            shift 2
            ;;
        --jobs=*)
            jobs=${1#--jobs=}
            shift
            ;;
        -*)
            printf 'error: unknown option %s\n' "$1" >&2
            usage
            ;;
        *)
            [ -z "$target" ] || die "only one target triple may be given"
            target=$1
            shift
            ;;
    esac
done

case "$jobs" in
    ''|*[!0-9]*) die "jobs must be a positive integer, got '$jobs'" ;;
esac
[ "$jobs" -ge 1 ] || die "jobs must be a positive integer, got '$jobs'"

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
root=$(CDPATH='' cd -- "$script_dir/.." && pwd -P)
cd "$root"

if [ -z "$target" ]; then
    rustc --version >/dev/null 2>&1 || die "rustc is not on PATH"
    target=$(rustc -vV | sed -n 's/^host: //p')
    [ -n "$target" ] || die "could not read the host triple from rustc -vV"
fi

case "$target" in
    *windows*) exe=.exe ;;
    *) exe= ;;
esac

target_root=${CARGO_TARGET_DIR:-$root/target}
case "$target_root" in
    /*) ;;
    *) target_root=$root/$target_root ;;
esac

stage_name=$package_name-$target
stage_dir=$root/dist/$stage_name
binary=$target_root/$target/release/$bin_name$exe
staged_binary=$stage_dir/bin/$bin_name$exe

printf 'building %s for %s (jobs=%s)\n' "$package_name" "$target" "$jobs"
cargo build --release --locked --target "$target" --jobs "$jobs"

[ -f "$binary" ] || die "cargo did not produce $binary"

mkdir -p "$stage_dir/bin"

stage_file() {
    source=$1
    destination=$2
    [ -f "$source" ] || die "missing $source"
    if [ -d "$destination" ]; then
        die "$destination is a directory"
    fi
    cp -f "$source" "$destination"
}

if [ -d "$staged_binary" ]; then
    die "$staged_binary is a directory"
fi
install -m 0755 "$binary" "$staged_binary"

stage_file "$root/plugin.toml" "$stage_dir/plugin.toml"
stage_file "$root/window.luau" "$stage_dir/window.luau"
stage_file "$root/README.md" "$stage_dir/README.md"
stage_file "$root/README.en.md" "$stage_dir/README.en.md"
stage_file "$root/DESIGN.md" "$stage_dir/DESIGN.md"

case "$target" in
    *-apple-darwin)
        command -v codesign >/dev/null 2>&1 \
            || die "codesign is required to sign a macOS binary; run this on macOS"
        codesign --force --sign - "$staged_binary"
        ;;
esac

mkdir -p "$root/dist"
case "$target" in
    *windows*)
        command -v zip >/dev/null 2>&1 \
            || die "zip is required to package a Windows target from this host"
        archive=$root/dist/$stage_name.zip
        rm -f "$archive"
        (cd "$root/dist" && zip -qry "$archive" "$stage_name")
        ;;
    *)
        archive=$root/dist/$stage_name.tar.gz
        tar -czf "$archive" -C "$root/dist" "$stage_name"
        ;;
esac

printf 'staged  %s\n' "$stage_dir"
printf 'archive %s\n' "$archive"
