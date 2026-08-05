#!/usr/bin/env bash
#
# Install a built solo-pool-rs binary so that deploys are deliberate.
#
# The problem this solves: /usr/local/bin/solo-pool-rs was a symlink straight
# into the repo's target/release. That directory is build output, so *any*
# cargo build in the working tree, by anyone, for any reason, silently changed
# which binary the service would start on its next restart. A test build is
# enough to arm an unplanned deploy, and because restarts also happen
# automatically after security patches, that deploy can happen without anyone
# choosing it. There was also no way to roll back: the previous binary was
# simply gone.
#
# What this does instead: copy the binary to a versioned path and point the
# symlink at it. Building never touches what is installed, upgrades are one
# command, and rollback is the same command against an older version.
#
#   sudo packaging/install.sh              # build and install the current version
#   sudo packaging/install.sh --rollback   # relink the previously installed one
#   packaging/install.sh --list            # show what is installed (no root)
#
# Restarting is left to you. Installing does not restart the pool.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIBDIR=/usr/local/lib/solo-pool-rs
LINK=/usr/local/bin/solo-pool-rs

version() { grep -m1 '^version' "$REPO/Cargo.toml" | cut -d'"' -f2; }

list() {
  echo "installed versions in $LIBDIR:"
  if [ -d "$LIBDIR" ]; then
    for d in "$LIBDIR"/*/; do
      [ -d "$d" ] || continue
      v="$(basename "$d")"
      marker=""
      [ "$(readlink -f "$LINK" 2>/dev/null)" = "$d/solo-pool-rs" ] && marker="  <- active"
      echo "  $v$marker"
    done
  else
    echo "  (none)"
  fi
  echo "current link: $LINK -> $(readlink -f "$LINK" 2>/dev/null || echo 'missing')"
}

need_root() {
  [ "$(id -u)" -eq 0 ] || { echo "error: needs root (re-run with sudo)" >&2; exit 1; }
}

# Point the symlink at $1 without ever leaving it dangling: create the new link
# beside the old one, then rename over it, which is atomic on the same fs.
relink() {
  ln -sfn "$1" "$LINK.new"
  mv -T "$LINK.new" "$LINK"
}

case "${1:-install}" in
  --list|list)
    list
    ;;

  --rollback|rollback)
    need_root
    current="$(readlink -f "$LINK" 2>/dev/null || true)"
    prev="$(find "$LIBDIR" -mindepth 1 -maxdepth 1 -type d | sort -V | \
            grep -v "^$(dirname "$current")$" | tail -1 || true)"
    [ -n "$prev" ] || { echo "error: no other installed version to roll back to" >&2; exit 1; }
    relink "$prev/solo-pool-rs"
    echo "rolled back to $(basename "$prev")"
    echo "run: systemctl restart solo-pool-rs"
    ;;

  install|--install)
    need_root
    v="$(version)"
    src="$REPO/target/release/solo-pool-rs"
    [ -x "$src" ] || { echo "error: $src not found; run 'cargo build --release' first" >&2; exit 1; }

    dest="$LIBDIR/$v"
    install -d "$dest"
    # Copy to a temp name then rename, so a partially-written binary is never
    # reachable through the symlink.
    install -m 0755 "$src" "$dest/solo-pool-rs.new"
    mv -T "$dest/solo-pool-rs.new" "$dest/solo-pool-rs"
    relink "$dest/solo-pool-rs"

    echo "installed $v -> $dest/solo-pool-rs"
    echo "$LINK -> $(readlink -f "$LINK")"
    echo
    echo "the running process keeps its old binary until you restart:"
    echo "  systemctl restart solo-pool-rs"
    ;;

  *)
    echo "usage: $0 [install|--rollback|--list]" >&2
    exit 2
    ;;
esac
