#!/usr/bin/env bash
# Rewrite the Homebrew formula in place for a new release: the `version` line
# plus the per-target `sha256` digests, read from the release's SHA256SUMS.
#
#   bump-homebrew-formula.sh <version> <sha256sums-file> <formula-file>
#
# Deliberately standalone and side-effect free (no git, no network, no
# secrets) so it can be run against a copy of the formula on a laptop; the
# workflow only supplies the arguments and decides whether to commit.
#
# Every substitution is checked: a formula that stops matching the patterns
# this script expects must fail the release loudly rather than silently ship a
# tap pointing at the previous version's tarballs.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <version> <sha256sums-file> <formula-file>" >&2
  exit 2
fi

version="$1"
sums_file="$2"
formula="$3"

# A leading `v` is accepted so callers can pass a tag name straight through.
version="${version#v}"

[ -f "$sums_file" ] || { echo "error: no such SHA256SUMS file: $sums_file" >&2; exit 1; }
[ -f "$formula" ] || { echo "error: no such formula: $formula" >&2; exit 1; }

# The targets whose digests appear in the formula. Windows ships a .zip and
# has no Homebrew bottle, so it is not listed here.
targets=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
)

# digest <target> — the sha256 of mcpgw-<version>-<target>.tar.gz.
digest() {
  local target="$1" asset line
  asset="mcpgw-${version}-${target}.tar.gz"
  # Match the whole filename field so `x86_64-apple-darwin` can never pick up
  # a line for a different target that merely contains it as a substring.
  line="$(awk -v a="$asset" '$2 == a || $2 == "*" a { print; n++ } END { exit n == 1 ? 0 : 1 }' "$sums_file")" ||
    { echo "error: expected exactly one entry for ${asset} in ${sums_file}" >&2; exit 1; }
  printf '%s' "${line%% *}"
}

declare -a sums=()
for target in "${targets[@]}"; do
  sum="$(digest "$target")"
  case "$sum" in
    [0-9a-f][0-9a-f]*) [ "${#sum}" -eq 64 ] || { echo "error: ${target} digest is not 64 hex chars: ${sum}" >&2; exit 1; } ;;
    *) echo "error: ${target} digest is not lowercase hex: ${sum}" >&2; exit 1 ;;
  esac
  sums+=("$sum")
done

# The rewrite keys off structure rather than line numbers: a `url` line names
# the target, and the `sha256` line that follows it belongs to that target.
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

awk -v version="$version" \
    -v targets="${targets[*]}" \
    -v sums="${sums[*]}" '
BEGIN {
  n = split(targets, t, " ")
  split(sums, s, " ")
  for (i = 1; i <= n; i++) sum[t[i]] = s[i]
}
# version "0.2.1"
match($0, /^([ \t]*)version "[^"]*"$/) {
  sub(/version "[^"]*"$/, "version \"" version "\"")
  versions++
  print
  next
}
# url "...-<target>.tar.gz"
/^[ \t]*url "/ {
  pending = ""
  for (target in sum) if (index($0, "-" target ".tar.gz")) pending = target
  print
  next
}
# sha256 "..." immediately under the url it belongs to
pending != "" && match($0, /^([ \t]*)sha256 "[^"]*"$/) {
  sub(/sha256 "[^"]*"$/, "sha256 \"" sum[pending] "\"")
  seen[pending] = 1
  replaced++
  pending = ""
  print
  next
}
{ print }
END {
  if (versions != 1) { printf "error: expected exactly 1 version line, found %d\n", versions > "/dev/stderr"; exit 1 }
  for (i = 1; i <= n; i++)
    if (!(t[i] in seen)) { printf "error: no sha256 line found for target %s\n", t[i] > "/dev/stderr"; exit 1 }
  if (replaced != n) { printf "error: replaced %d sha256 lines, expected %d\n", replaced, n > "/dev/stderr"; exit 1 }
}
' "$formula" > "$tmp"

cat "$tmp" > "$formula"

# Independent check of the result, so a bug in the rewrite above cannot ship.
grep -qx "[[:space:]]*version \"${version}\"" "$formula" ||
  { echo "error: formula does not contain version \"${version}\" after rewrite" >&2; exit 1; }
for i in "${!targets[@]}"; do
  grep -qx "[[:space:]]*sha256 \"${sums[$i]}\"" "$formula" ||
    { echo "error: formula is missing the ${targets[$i]} digest after rewrite" >&2; exit 1; }
done

echo "formula updated to ${version}"
for i in "${!targets[@]}"; do
  echo "  ${targets[$i]} ${sums[$i]}"
done
