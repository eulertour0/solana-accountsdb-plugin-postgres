#!/usr/bin/env bash
#
# Verifies that this plugin resolves every crate it shares with the validator to
# the same version as the agave source tree the validator is built from, and
# lists crates whose enabled feature sets differ (review those that define types
# crossing the plugin boundary).
#
#   scripts/check_agave_abi_deps.sh [AGAVE_DIR]
#
# AGAVE_DIR defaults to ../agave-v4.3.0 (the path dependency in Cargo.toml).
# Needs only `cargo tree`; nothing is built. Exit status 1 on a version mismatch
# outside the reviewed allowlist below.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
agave="$(cd "${1:-$here/../agave-v4.3.0}" && pwd)"
expected_rev=825efd18

if git -C "$agave" rev-parse HEAD >/dev/null 2>&1; then
  rev="$(git -C "$agave" rev-parse HEAD)"
  case "$rev" in
    "$expected_rev"*) echo "agave source: $agave @ $rev (v4.3.0)" ;;
    *) echo "WARNING: agave source $agave is at $rev, expected $expected_rev (v4.3.0)" ;;
  esac
fi
if [[ "$(readlink -f "$agave")" != "$(readlink -f "$here/../agave-v4.3.0")" ]]; then
  echo "WARNING: Cargo.toml path dependencies point at $here/../agave-v4.3.0, not $agave"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

tree() {
  cargo tree --locked --offline -e normal -f '{p}|{f}' --prefix none "$@" 2>/dev/null |
    sed 's/ ([^)]*)//g' | sort -u
}
(cd "$agave" && tree -p agave-validator) >"$tmp/validator"
(cd "$here" && tree) >"$tmp/plugin"

python3 - "$tmp/validator" "$tmp/plugin" <<'PY'
import sys

def load(path):
    packages = {}
    for line in open(path):
        line = line.strip()
        if "|" not in line:
            continue
        package, features = line.split("|", 1)
        name, version = package.split(" ")[:2]
        packages.setdefault(name, {})[version] = {f for f in features.split(",") if f}
    return packages

# Reviewed differences; none of these crates has types in the plugin interface.
#   const-oid, hmac: extra newer versions pulled only by postgres-protocol.
#   crossbeam-epoch: agave patches it to an anza-xyz git fork in its workspace
#     [patch.crates-io]; patches do not apply to dependents. Used only by rayon.
ALLOWED = {"const-oid", "hmac", "crossbeam-epoch"}

validator, plugin = load(sys.argv[1]), load(sys.argv[2])
mismatches = []
feature_diffs = []
for name in sorted(set(validator) & set(plugin)):
    for version, features in plugin[name].items():
        if version not in validator[name]:
            mismatches.append(f"{name} {version} (validator: {', '.join(sorted(validator[name]))})")
        elif features != validator[name][version]:
            only_plugin = sorted(features - validator[name][version])
            only_validator = sorted(validator[name][version] - features)
            feature_diffs.append(f"{name} {version}: plugin-only {only_plugin} validator-only {only_validator}")

print(f"shared crates: {len(set(validator) & set(plugin))}")
print("feature differences (informational):")
for line in feature_diffs:
    print("  " + line)
allowed = [m for m in mismatches if m.split(" ")[0] in ALLOWED]
rejected = [m for m in mismatches if m.split(" ")[0] not in ALLOWED]
for line in allowed:
    print("allowed version difference: " + line)
for line in rejected:
    print("VERSION MISMATCH: " + line)
if rejected:
    sys.exit(1)
print("OK: all other shared crates resolve to the validator's versions")
PY
