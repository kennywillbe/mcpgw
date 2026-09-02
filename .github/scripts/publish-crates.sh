#!/usr/bin/env bash
# Publish the workspace's public crates to crates.io, in dependency order.
#
#   publish-crates.sh <version>
#
# `mcpgw` depends on `mcpgw-core` by version as well as by path, so core has
# to be on the registry — and visible in the index — before the CLI is
# uploaded. crates/test-server is `publish = false` and is never named here.
#
# Idempotent on purpose: re-running the release workflow for a tag whose
# crates are already on crates.io should be a no-op, not a red run. A version
# that is already uploaded is success; anything else is fatal.
#
# The token is never read by this script — cargo picks up CARGO_REGISTRY_TOKEN
# from the environment itself, so it stays out of the argv and out of the log.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <version>" >&2
  exit 2
fi

# A leading `v` is accepted so callers can pass a tag name straight through.
version="${1#v}"

# Publishing whatever happens to be in the manifests would, for a tag that
# does not match them, put the wrong version on crates.io under the right
# release notes — and crates.io has no undo. Fail before the first upload.
for manifest in crates/core/Cargo.toml crates/cli/Cargo.toml; do
  manifest_version="$(awk -F'"' '/^version = "/ { print $2; exit }' "$manifest")"
  if [ "$manifest_version" != "$version" ]; then
    echo "::error::${manifest} is at ${manifest_version} but the tag says ${version}; refusing to publish." >&2
    exit 1
  fi
done

# The sparse index lays crate files out by name length; see the cargo book's
# "Registry index" chapter. Only ever called with our own lowercase names.
index_path() {
  local name="$1"
  case "${#name}" in
    1) printf '1/%s' "$name" ;;
    2) printf '2/%s' "$name" ;;
    3) printf '3/%s/%s' "${name:0:1}" "$name" ;;
    *) printf '%s/%s/%s' "${name:0:2}" "${name:2:2}" "$name" ;;
  esac
}

# A successful `cargo publish` only means crates.io accepted the tarball: the
# index it serves to cargo catches up a moment later. Publishing the CLI
# before then fails to resolve mcpgw-core, so wait for the index rather than
# guessing at a sleep.
wait_for_index() {
  local name="$1" want="$2" url deadline body
  url="https://index.crates.io/$(index_path "$name")"
  deadline=$(( SECONDS + 300 ))
  while [ "$SECONDS" -lt "$deadline" ]; do
    # `|| true`: a 404 is the normal answer for a crate the index has not
    # caught up to yet, and curl's failure there is not this script's.
    body="$(curl -fsSL --max-time 30 -H 'Cache-Control: no-cache' "$url" || true)"
    if printf '%s' "$body" | grep -q "\"vers\"[[:space:]]*:[[:space:]]*\"${want}\""; then
      echo "index has ${name} ${want}"
      return 0
    fi
    echo "waiting for ${name} ${want} to appear in the index…"
    sleep 10
  done
  echo "::error::${name} ${want} did not appear in the crates.io index within 5 minutes." >&2
  exit 1
}

# publish <package> [attempts] — upload one crate, treating an already
# uploaded version as success and retrying the resolver failures that mean
# the registry is still catching up.
publish() {
  local pkg="$1" attempts="${2:-1}" attempt=1 log
  log="$(mktemp)"
  while :; do
    echo "publishing ${pkg} ${version} (attempt ${attempt}/${attempts})"
    if cargo publish --locked -p "$pkg" 2>&1 | tee "$log"; then
      rm -f "$log"
      return 0
    fi
    if grep -q 'is already uploaded' "$log"; then
      echo "${pkg} ${version} is already on crates.io; nothing to upload."
      rm -f "$log"
      return 0
    fi
    # The registry serving a stale index is the one failure worth retrying:
    # everything else (a lint, a bad manifest, a rejected token) is fatal and
    # retrying it just makes the log longer.
    if [ "$attempt" -lt "$attempts" ] &&
       grep -qE 'no matching package|failed to select a version' "$log"; then
      echo "registry has not caught up yet; retrying in 30s"
      attempt=$(( attempt + 1 ))
      sleep 30
      continue
    fi
    rm -f "$log"
    echo "::error::cargo publish -p ${pkg} failed." >&2
    exit 1
  done
}

publish mcpgw-core
wait_for_index mcpgw-core "$version"
publish mcpgw 5

echo "crates.io is at ${version}"
