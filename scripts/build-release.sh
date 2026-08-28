#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

[[ $(uname -m) == x86_64 ]] || {
  echo "Linux releases currently support x86_64 only" >&2
  exit 1
}

version=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c '
import json, sys
print(next(package["version"] for package in json.load(sys.stdin)["packages"] if package["name"] == "z13helper"))
')
target_dir=${CARGO_TARGET_DIR:-$root/target}
dist_dir=${DIST_DIR:-$root/dist}
release="z13helper-$version-x86_64-linux"
stage=$(mktemp -d "${TMPDIR:-/tmp}/z13helper-release.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT

cargo build --release --locked -p z13helper -p z13helperd -p z13helperctl

install -Dm755 "$target_dir/release/z13helper" "$dist_dir/z13helper"
install -Dm755 "$target_dir/release/z13helperctl" "$dist_dir/z13helperctl"
install -Dm755 "$target_dir/release/z13helperd" "$dist_dir/z13helperd"

rootfs="$stage/$release"
install -Dm755 "$target_dir/release/z13helper" "$rootfs/usr/bin/z13helper"
install -Dm755 "$target_dir/release/z13helperctl" "$rootfs/usr/bin/z13helperctl"
install -Dm755 "$target_dir/release/z13helperd" "$rootfs/usr/libexec/z13helperd"
install -Dm644 contrib/com.ashtonantila.z13helper.desktop \
  "$rootfs/usr/share/applications/com.ashtonantila.z13helper.desktop"
install -Dm644 assets/z13helper.svg \
  "$rootfs/usr/share/icons/hicolor/scalable/apps/z13helper.svg"
install -Dm644 contrib/z13helper.service "$rootfs/usr/lib/systemd/user/z13helper.service"
install -Dm644 contrib/systemd/z13helperd.service "$rootfs/usr/lib/systemd/system/z13helperd.service"
install -Dm644 contrib/sysusers.d/z13helper.conf "$rootfs/usr/lib/sysusers.d/z13helper.conf"
install -Dm644 LICENSE "$rootfs/usr/share/licenses/z13helper/LICENSE"
[[ ! -d LICENSES ]] || cp -a LICENSES/. "$rootfs/usr/share/licenses/z13helper/"

tar -C "$stage" -czf "$dist_dir/$release.tar.gz" "$release"
(
  cd "$dist_dir"
  sha256sum z13helper z13helperctl z13helperd "$release.tar.gz" > "$release.sha256"
)
