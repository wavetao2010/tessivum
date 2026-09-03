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
    ln -s tessivum "$build/$archive_root/bin/tsv"
    tar -C "$build" -czf "$archive" "$archive_root"
    (cd "$work" && checksum "$(basename "$archive")" > "$archive.sha256")
    fixture_unpack="$work/unpack-$version-$target"
    rm -rf "$fixture_unpack"
    mkdir -p "$fixture_unpack"
    tar -xzf "$archive" -C "$fixture_unpack"
    assert_link "$fixture_unpack/$archive_root/bin/tsv" tessivum
    rm -rf "$fixture_unpack"
    printf '%s\n' "$archive"
}

run_installer() {
    fixture=$1
    version=$2
    prefix=$3
    os=$4
    arch=$5

    HOME="$prefix/home" \
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
    HOME="$prefix/home" \
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

assert_absent() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        fail "path remains: $1"
    fi
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
    assert_link "$prefix/bin/tsv" "$prefix/lib/tessivum/$version/bin/tessivum"
    [ "$("$prefix/bin/tessivum")" = "$target" ] || fail "wrong executable for $target"
    [ "$("$prefix/bin/tsv")" = "$target" ] || fail "wrong alias executable for $target"
}

test_collision() {
    link_name=$1
    collision_kind=$2
    version="1.0.0-$link_name-$collision_kind"
    prefix="$work/collision-$link_name-$collision_kind"
    fixture=$(fixture_archive "$version" "$target" collision)
    collision="$prefix/bin/$link_name"
    third_party="$work/third-party-$link_name-$collision_kind"
    mkdir -p "$prefix/bin" "$third_party"

    case "$collision_kind" in
        regular) printf '%s\n' external > "$collision" ;;
        symlink)
            printf '%s\n' external > "$third_party/tool"
            ln -s "$third_party/tool" "$collision"
            ;;
        dangling) ln -s "$third_party/missing" "$collision" ;;
        traversal)
            mkdir -p "$prefix/lib/bin"
            printf '%s\n' external > "$prefix/lib/bin/tessivum"
            ln -s "$prefix/lib/tessivum/../bin/tessivum" "$collision"
            ;;
        directory) mkdir "$collision" ;;
    esac

    if run_installer "$fixture" "$version" "$prefix" Linux x86_64; then
        fail "installer replaced $collision_kind collision at $collision"
    fi
    assert_absent "$prefix/lib/tessivum/$version"
    assert_no_partial "$prefix/lib/tessivum"
    case "$collision_kind" in
        regular) [ "$(cat "$collision")" = external ] || fail "regular collision changed" ;;
        symlink)
            assert_link "$collision" "$third_party/tool"
            [ "$(cat "$third_party/tool")" = external ] || fail "external target changed"
            ;;
        dangling) assert_link "$collision" "$third_party/missing" ;;
        traversal)
            assert_link "$collision" "$prefix/lib/tessivum/../bin/tessivum"
            [ "$(cat "$prefix/lib/bin/tessivum")" = external ] || fail "escaping target changed"
            ;;
        directory) [ -d "$collision" ] || fail "directory collision changed" ;;
    esac
    if [ "$link_name" = tessivum ]; then
        assert_absent "$prefix/bin/tsv"
    else
        assert_absent "$prefix/bin/tessivum"
    fi
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
old_version=0.1.0-alpha.20
new_version=0.1.0-alpha.21
blocked_version=1.0.0-blocked
old_fixture=$(fixture_archive "$old_version" "$target" old)
new_fixture=$(fixture_archive "$new_version" "$target" new)
blocked_fixture=$(fixture_archive "$blocked_version" "$target" blocked)
run_installer "$old_fixture" "$old_version" "$prefix" Linux x86_64
data_root="$prefix/home/.tessivum"
mkdir -p "$data_root"
printf '%s\n' alpha19-durable-state > "$data_root/state.marker"
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$old_version/bin/tessivum"
assert_link "$prefix/bin/tsv" "$prefix/lib/tessivum/$old_version/bin/tessivum"
old_target=$(readlink "$prefix/bin/tessivum")

rm "$prefix/bin/tsv"
printf '%s\n' external > "$prefix/bin/tsv"
if run_installer "$blocked_fixture" "$blocked_version" "$prefix" Linux x86_64; then
    fail "alias collision allowed a partial upgrade"
fi
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$old_version/bin/tessivum"
[ "$("$prefix/bin/tessivum")" = old ] || fail "alias collision replaced executable"
[ "$(cat "$prefix/bin/tsv")" = external ] || fail "alias collision changed third-party path"
assert_absent "$prefix/lib/tessivum/$blocked_version"
assert_no_partial "$prefix/lib/tessivum"
[ "$(cat "$data_root/state.marker")" = alpha19-durable-state ] || fail "blocked upgrade changed Alpha19 durable state"

rm "$prefix/bin/tsv"
run_installer "$new_fixture" "$new_version" "$prefix" Linux x86_64
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$new_version/bin/tessivum"
assert_link "$prefix/bin/tsv" "$prefix/lib/tessivum/$new_version/bin/tessivum"
[ "$("$prefix/bin/tessivum")" = new ] || fail "upgrade did not switch executable"
[ "$("$prefix/bin/tsv")" = new ] || fail "upgrade did not switch alias executable"
[ "$old_target" != "$(readlink "$prefix/bin/tessivum")" ] || fail "upgrade did not replace symlink"
[ "$(cat "$data_root/state.marker")" = alpha19-durable-state ] || fail "Alpha20 upgrade changed Alpha19 durable state"

failed_version=1.0.0-failed
failed_fixture=$(fixture_archive "$failed_version" "$target" failed)
printf '%064d  %s\n' 0 "$(basename "$failed_fixture")" > "$failed_fixture.sha256"
if run_installer "$failed_fixture" "$failed_version" "$prefix" Linux x86_64; then
    fail "failed upgrade installed an archive"
fi
assert_link "$prefix/bin/tessivum" "$prefix/lib/tessivum/$new_version/bin/tessivum"
assert_link "$prefix/bin/tsv" "$prefix/lib/tessivum/$new_version/bin/tessivum"
[ "$("$prefix/bin/tessivum")" = new ] || fail "failed upgrade replaced executable"
assert_absent "$prefix/lib/tessivum/$failed_version"
assert_no_partial "$prefix/lib/tessivum"
[ "$(cat "$data_root/state.marker")" = alpha19-durable-state ] || fail "failed upgrade changed Alpha19 durable state"

run_uninstaller "$prefix"
assert_absent "$prefix/bin/tessivum"
assert_absent "$prefix/bin/tsv"
assert_absent "$prefix/lib/tessivum"
[ "$(cat "$data_root/state.marker")" = alpha19-durable-state ] || fail "uninstall removed durable user data"
run_uninstaller "$prefix"

prefix="$work/uninstall-external"
version=1.0.0-uninstall-external
fixture=$(fixture_archive "$version" "$target" external)
third_party="$work/uninstall-external-tool"
mkdir -p "$third_party"
printf '%s\n' external > "$third_party/tool"
run_installer "$fixture" "$version" "$prefix" Linux x86_64
rm "$prefix/bin/tsv"
ln -s "$third_party/tool" "$prefix/bin/tsv"
run_uninstaller "$prefix"
assert_absent "$prefix/bin/tessivum"
assert_absent "$prefix/lib/tessivum"
assert_link "$prefix/bin/tsv" "$third_party/tool"
[ "$(cat "$third_party/tool")" = external ] || fail "uninstall changed third-party target"

prefix="$work/uninstall-canonical"
version=1.0.0-uninstall-canonical
fixture=$(fixture_archive "$version" "$target" canonical)
run_installer "$fixture" "$version" "$prefix" Linux x86_64
rm "$prefix/bin/tessivum"
printf '%s\n' external > "$prefix/bin/tessivum"
if run_uninstaller "$prefix"; then
    fail "uninstall removed an unmanaged canonical executable"
fi
[ "$(cat "$prefix/bin/tessivum")" = external ] || fail "uninstall changed canonical collision"
assert_link "$prefix/bin/tsv" "$prefix/lib/tessivum/$version/bin/tessivum"
[ -e "$prefix/lib/tessivum" ] || fail "uninstall removed files without canonical ownership"

test_collision tessivum regular
test_collision tsv regular
test_collision tsv symlink
test_collision tsv dangling
test_collision tsv traversal
test_collision tsv directory

printf '%s\n' 'installer fixtures passed'
