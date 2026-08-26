#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
installer="$root/install.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/tessivum-install-test.XXXXXX")
trap 'rm -rf "$work"' 0 HUP INT TERM
fake_bin="$work/fake-bin"
mkdir -p "$fake_bin"

cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) printf '%s\n' "$TESSIVUM_TEST_UNAME_S" ;;
    -m) printf '%s\n' "$TESSIVUM_TEST_UNAME_M" ;;
    *) exit 1 ;;
esac
EOF
chmod +x "$fake_bin/uname"

fail() {
    printf '%s\n' "test_install.sh: $*" >&2
    exit 1
}

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1"
    else
        fail "sha256sum or shasum is required"
    fi
}

fixture_archive() {
    version=$1
    target=$2
    label=$3
    archive_root=${4:-tessivum-$version-$target}
    build="$work/build-$version-$target"
    archive="$work/tessivum-$version-$target.tar.gz"

    rm -rf "$build"
    mkdir -p "$build/$archive_root/bin"
    printf '%s\n' '#!/bin/sh' "printf '%s\n' '$label'" > "$build/$archive_root/bin/tessivum"
    chmod +x "$build/$archive_root/bin/tessivum"
    tar -C "$build" -czf "$archive" "$archive_root"
    (cd "$work" && checksum "$(basename "$archive")" > "$archive.sha256")
    printf '%s\n' "$archive"
}

run_installer() {
    fixture=$1
    version=$2
    prefix=$3
    os=$4
    arch=$5

    PATH="$fake_bin:$PATH" \
    TESSIVUM_INSTALLER_TEST=1 \
    TESSIVUM_TEST_UNAME_S="$os" \
    TESSIVUM_TEST_UNAME_M="$arch" \
    FIXTURE_URL="file://$fixture" \
    VERSION="$version" \
    INSTALL_ROOT="$prefix/lib/tessivum" \
    BIN_DIR="$prefix/bin" \
    sh "$installer"
}

run_uninstaller() {
    prefix=$1
    INSTALL_ROOT="$prefix/lib/tessivum" \
    BIN_DIR="$prefix/bin" \
    sh "$installer" --uninstall
}

assert_link() {
    link_path=$1
    expected_target=$2
    [ -L "$link_path" ] || fail "missing symlink: $link_path"
    [ "$(readlink "$link_path")" = "$expected_target" ] || fail "wrong symlink target: $link_path"
}
assert_no_partial() {
    install_root=$1
    for partial in "$install_root"/.tessivum-install.*; do
        [ ! -e "$partial" ] || fail "partial install remains: $partial"
    done
}


test_mapping() {
    os=$1
    arch=$2
    target=$3
    version=1.0.0-map
    prefix="$work/$target"
    fixture=$(fixture_archive "$version" "$target" "$target")

    run_installer "$fixture" "$version" "$prefix" "$os" "$arch"
    assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$version/bin/tessivum"
    [ "$("$prefix/bin/tessivum")" = "$target" ] || fail "wrong executable for $target"
}

test_mapping Linux x86_64 x86_64-unknown-linux-gnu
test_mapping Linux aarch64 aarch64-unknown-linux-gnu
test_mapping Darwin x86_64 x86_64-apple-darwin
test_mapping Darwin arm64 aarch64-apple-darwin

target=x86_64-unknown-linux-gnu
prefix="$work/failures"
version=1.0.0-checksum
fixture=$(fixture_archive "$version" "$target" checksum)
printf '%064d  %s\n' 0 "$(basename "$fixture")" > "$fixture.sha256"
if run_installer "$fixture" "$version" "$prefix" Linux x86_64; then
    fail "checksum failure installed an archive"
fi
[ ! -e "$prefix/lib/tessivum/$version" ] || fail "checksum failure left an install"
assert_no_partial "$prefix/lib/tessivum"

version=1.0.0-root
wrong_root=tessivum-1.0.0-other-$target
fixture=$(fixture_archive "$version" "$target" root-mismatch "$wrong_root")
if run_installer "$fixture" "$version" "$prefix" Linux x86_64; then
    fail "root mismatch installed an archive"
fi
[ ! -e "$prefix/lib/tessivum/$version" ] || fail "root mismatch left an install"
assert_no_partial "$prefix/lib/tessivum"

prefix="$work/upgrade"
old_version=1.0.0-old
new_version=1.0.0-new
old_fixture=$(fixture_archive "$old_version" "$target" old)
new_fixture=$(fixture_archive "$new_version" "$target" new)
run_installer "$old_fixture" "$old_version" "$prefix" Linux x86_64
old_target=$(readlink "$prefix/bin/tessivum")
run_installer "$new_fixture" "$new_version" "$prefix" Linux x86_64
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$new_version/bin/tessivum"
[ "$("$prefix/bin/tessivum")" = new ] || fail "upgrade did not switch executable"
[ "$old_target" != "$(readlink "$prefix/bin/tessivum")" ] || fail "upgrade did not replace symlink"

failed_version=1.0.0-failed
failed_fixture=$(fixture_archive "$failed_version" "$target" failed)
printf '%064d  %s\n' 0 "$(basename "$failed_fixture")" > "$failed_fixture.sha256"
if run_installer "$failed_fixture" "$failed_version" "$prefix" Linux x86_64; then
    fail "failed upgrade installed an archive"
fi
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$new_version/bin/tessivum"
[ "$("$prefix/bin/tessivum")" = new ] || fail "failed upgrade replaced executable"
[ ! -e "$prefix/lib/tessivum/$failed_version" ] || fail "failed upgrade left an install"
assert_no_partial "$prefix/lib/tessivum"

run_uninstaller "$prefix"
[ ! -e "$prefix/bin/tessivum" ] || fail "uninstall left the executable link"
[ ! -e "$prefix/lib/tessivum" ] || fail "uninstall left versioned installs"
run_uninstaller "$prefix"

printf '%s\n' 'installer fixtures passed'
