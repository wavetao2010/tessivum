#!/bin/sh
set -eu

usage() {
    printf '%s\n' "usage: $0 [version|--uninstall]" >&2
    exit 2
}

die() {
    printf '%s\n' "install.sh: $*" >&2
    exit 1
}

[ "$#" -le 1 ] || usage

uninstall=
if [ "${1:-}" = --uninstall ]; then
    uninstall=1
elif [ "${1:-}" != "" ]; then
    case "$1" in -*) usage ;; esac
fi

version=${1:-${VERSION:-0.1.0-alpha.14}}
repository=${REPOSITORY:-https://github.com/wavetao2010/tessivum}
fixture_url=${FIXTURE_URL:-}
test_mode=${TESSIVUM_INSTALLER_TEST:-}

if [ -z "$uninstall" ]; then
    case "$version" in
        ''|.*|*..*|*[!0-9A-Za-z.-]*) die "invalid version: $version" ;;
    esac
fi

home=${HOME:-}
if [ -n "${INSTALL_ROOT:-}" ]; then
    install_root="$INSTALL_ROOT"
elif [ -n "$home" ]; then
    install_root="$home/.local/lib/tessivum"
else
    die "HOME is required unless INSTALL_ROOT is set"
fi

if [ -n "${BIN_DIR:-}" ]; then
    bin_dir="$BIN_DIR"
elif [ -n "$home" ]; then
    bin_dir="$home/.local/bin"
else
    die "HOME is required unless BIN_DIR is set"
fi

case "$install_root" in
    /*) ;;
    *) install_root="$(pwd -P)/$install_root" ;;
esac
case "$bin_dir" in
    /*) ;;
    *) bin_dir="$(pwd -P)/$bin_dir" ;;
esac
[ "$install_root" != / ] || die "INSTALL_ROOT must not be /"
[ "$bin_dir" != / ] || die "BIN_DIR must not be /"
is_managed_link() {
    [ -L "$1" ] || return 1
    link_target=$(readlink "$1") || return 1
    case "$link_target" in
        "$install_root"/*/bin/tessivum)
            link_version=${link_target#"$install_root"/}
            link_version=${link_version%/bin/tessivum}
            case "$link_version" in
                ''|.*|*..*|*[!0-9A-Za-z.-]*) return 1 ;;
            esac
            [ "$link_target" = "$install_root/$link_version/bin/tessivum" ] || return 1
            return 0
            ;;
    esac
    return 1
}

can_replace_link() {
    if [ ! -e "$1" ] && [ ! -L "$1" ]; then
        return 0
    fi
    is_managed_link "$1"
}

if [ -n "$uninstall" ]; then
    canonical_link="$bin_dir/tessivum"
    alias_link="$bin_dir/tsv"
    if [ -L "$canonical_link" ]; then
        is_managed_link "$canonical_link" \
            || die "refusing to remove an executable not installed under $install_root"
    elif [ -e "$canonical_link" ]; then
        die "refusing to remove non-symlink executable $canonical_link"
    elif [ -e "$install_root" ] || [ -L "$install_root" ]; then
        die "refusing to remove $install_root without its managed executable link"
    else
        printf 'Tessivum is already uninstalled from %s\n' "$install_root"
        exit 0
    fi
    if [ -L "$alias_link" ] && is_managed_link "$alias_link"; then
        rm "$alias_link" || die "cannot remove $alias_link"
    fi
    rm "$canonical_link" || die "cannot remove $canonical_link"
    rm -rf "$install_root" || die "cannot remove $install_root"
    printf 'Uninstalled Tessivum from %s\n' "$install_root"
    exit 0
fi

case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin:x86_64|Darwin:amd64) target=x86_64-apple-darwin ;;
    Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
    *) die "unsupported platform: $(uname -s) $(uname -m)" ;;
esac

archive_name=tessivum-$version-$target.tar.gz
archive_root=tessivum-$version-$target
if [ -n "$fixture_url" ]; then
    [ "$test_mode" = 1 ] || die "FIXTURE_URL is only allowed with TESSIVUM_INSTALLER_TEST=1"
    archive_url="$fixture_url"
else
    archive_url="${repository%/}/releases/download/v$version/$archive_name"
fi

case "$archive_url" in
    https://*) ;;
    *) [ -n "$fixture_url" ] && [ "$test_mode" = 1 ] || die "release downloads must use HTTPS" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
if command -v sha256sum >/dev/null 2>&1; then
    checksum_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum_tool=shasum
else
    die "sha256sum or shasum is required"
fi

mkdir -p "$install_root" "$bin_dir" || die "cannot create install directories"
staging=$(mktemp -d "$install_root/.tessivum-install.XXXXXX") || die "cannot create staging directory"
link_staging=
installed_destination=
links_pending=
tessivum_replaced=
tsv_replaced=
cleanup() {
    if [ -n "$links_pending" ]; then
        [ -z "$tessivum_replaced" ] || rm -f "$bin_dir/tessivum"
        [ -z "$tsv_replaced" ] || rm -f "$bin_dir/tsv"
        [ ! -L "$link_staging/previous-tessivum" ] \
            || mv "$link_staging/previous-tessivum" "$bin_dir/tessivum" || :
        [ ! -L "$link_staging/previous-tsv" ] \
            || mv "$link_staging/previous-tsv" "$bin_dir/tsv" || :
    fi
    rm -rf "$staging"
    [ -z "$link_staging" ] || rm -rf "$link_staging"
    [ -z "$installed_destination" ] || rm -rf "$installed_destination"
}
trap cleanup 0 HUP INT TERM

archive="$staging/$archive_name"
checksum="$archive.sha256"
curl --fail --location --silent --show-error --output "$archive" "$archive_url" \
    || die "failed to download $archive_url"
curl --fail --location --silent --show-error --output "$checksum" "$archive_url.sha256" \
    || die "failed to download $archive_url.sha256"

IFS=' ' read -r expected_hash expected_file < "$checksum" || die "invalid checksum file"
case "$expected_hash" in
    *[!0123456789abcdefABCDEF]*|'') die "invalid checksum file" ;;
esac
[ "${#expected_hash}" -eq 64 ] || die "invalid checksum file"
case "$expected_file" in
    "$archive_name"|"*$archive_name") ;;
    *) die "checksum is not for $archive_name" ;;
esac
if [ "$checksum_tool" = sha256sum ]; then
    (cd "$staging" && sha256sum -c "$archive_name.sha256" >/dev/null) || die "checksum verification failed"
else
    (cd "$staging" && shasum -a 256 -c "$archive_name.sha256" >/dev/null) || die "checksum verification failed"
fi

contents="$staging/archive.contents"
tar -tzf "$archive" > "$contents" || die "invalid archive"
while IFS= read -r entry; do
    case "$entry" in
        "$archive_root"|"$archive_root"/*) ;;
        *) die "archive root must be $archive_root" ;;
    esac
    relative=${entry#"$archive_root"}
    case "$relative" in
        ..|../*|*/../*|*/..|.|./*|*/./*|*/.) die "unsafe archive path" ;;
    esac
done < "$contents"

tar -xzf "$archive" -C "$staging" || die "failed to extract archive"
[ -x "$staging/$archive_root/bin/tessivum" ] || die "archive does not contain executable bin/tessivum"

destination="$install_root/$version"
if [ -e "$destination" ]; then
    [ -x "$destination/bin/tessivum" ] || die "existing install is invalid: $destination"
fi

for executable_name in tessivum tsv; do
    can_replace_link "$bin_dir/$executable_name" \
        || die "refusing to replace unmanaged link $bin_dir/$executable_name"
done

if [ ! -e "$destination" ]; then
    mv "$staging/$archive_root" "$destination" || die "failed to install $version"
    installed_destination=$destination
fi

link_staging=$(mktemp -d "$bin_dir/.tessivum-link.XXXXXX") || die "cannot stage executable links"
for executable_name in tessivum tsv; do
    ln -s "$destination/bin/tessivum" "$link_staging/$executable_name" \
        || die "cannot create executable link"
done

links_pending=1
if [ -L "$bin_dir/tessivum" ]; then
    mv "$bin_dir/tessivum" "$link_staging/previous-tessivum" \
        || die "cannot stage existing executable link"
fi
if [ -L "$bin_dir/tsv" ]; then
    mv "$bin_dir/tsv" "$link_staging/previous-tsv" \
        || die "cannot stage existing executable link"
fi
mv "$link_staging/tessivum" "$bin_dir/tessivum" || die "cannot update executable link"
tessivum_replaced=1
mv "$link_staging/tsv" "$bin_dir/tsv" || die "cannot update executable link"
tsv_replaced=1
links_pending=
installed_destination=
rm -rf "$link_staging" || die "cannot remove executable link staging"
link_staging=

printf 'Installed Tessivum %s for %s at %s\n' "$version" "$target" "$bin_dir/tessivum"
