#!/bin/sh
set -eu

repository='soyuo/star.lang'
version=${1:-latest}
install_dir=${2:-${STAR_INSTALL_DIR:-"$HOME/.local/bin"}}

case "$version" in
    latest) release_url="https://github.com/$repository/releases/latest/download" ;;
    v[0-9A-Za-z._+-]*)
        case "$version" in *[!A-Za-z0-9._+-]*) echo "Invalid release tag: $version" >&2; exit 2 ;; esac
        release_url="https://github.com/$repository/releases/download/$version" ;;
    *) echo 'Usage: install.sh [vVERSION] [install-directory]' >&2; exit 2 ;;
esac

system=$(uname -s)
machine=$(uname -m)
case "$system:$machine" in
    Linux:x86_64|Linux:amd64) target='x86_64-unknown-linux-gnu' ;;
    Linux:aarch64|Linux:arm64) target='aarch64-unknown-linux-gnu' ;;
    Darwin:x86_64|Darwin:amd64) target='x86_64-apple-darwin' ;;
    Darwin:arm64|Darwin:aarch64) target='aarch64-apple-darwin' ;;
    *) echo "Unsupported platform: $system $machine" >&2; exit 2 ;;
esac

archive="star-$target.tar.gz"
temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM

curl --fail --location --silent --show-error "$release_url/$archive" -o "$temporary_dir/$archive"
curl --fail --location --silent --show-error "$release_url/SHA256SUMS" -o "$temporary_dir/SHA256SUMS"

expected_hash=$(awk -v name="$archive" '$2 == name { print $1 }' "$temporary_dir/SHA256SUMS")
if [ -z "$expected_hash" ]; then
    echo "No checksum found for $archive" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "$temporary_dir/$archive" | awk '{ print $1 }')
else
    actual_hash=$(shasum -a 256 "$temporary_dir/$archive" | awk '{ print $1 }')
fi
if [ "$actual_hash" != "$expected_hash" ]; then
    echo "Checksum verification failed for $archive" >&2
    exit 1
fi

if ! tar -tvzf "$temporary_dir/$archive" > "$temporary_dir/archive-listing"; then
    echo 'Could not read the release archive.' >&2
    exit 1
fi
awk '
    {
        type = substr($0, 1, 1)
        name = $NF
        if (type != "-") {
            print "Release archive members must be regular files." > "/dev/stderr"
            exit 1
        }
        if (name != "star" && name != "LICENSE" && name != "README.md") {
            print "Unexpected archive entry: " name > "/dev/stderr"
            exit 1
        }
        if (seen[name]++) {
            print "Duplicate archive entry: " name > "/dev/stderr"
            exit 1
        }
        count++
    }
    END {
        if (count != 3 || !seen["star"] || !seen["LICENSE"] || !seen["README.md"]) {
            print "The release archive does not contain the expected three files." > "/dev/stderr"
            exit 1
        }
    }
' "$temporary_dir/archive-listing"

mkdir -p "$temporary_dir/extracted" "$install_dir"
tar -xzf "$temporary_dir/$archive" -C "$temporary_dir/extracted"
install -m 0755 "$temporary_dir/extracted/star" "$install_dir/star"
printf 'Installed Star to %s/star\n' "$install_dir"
case ":$PATH:" in *":$install_dir:"*) ;;
    *) printf 'Add this directory to your PATH: %s\n' "$install_dir" ;;
esac
