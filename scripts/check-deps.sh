#!/usr/bin/env bash
# Enforces the dependency rule (ARCHITECTURE.md § 1):
#   * nixxa-contracts depends on no workspace crate;
#   * every other crate depends only on nixxa-contracts;
#   * nixxa-harness (the composition root) is the only exception.
# Covers normal, dev and build dependencies, and any crate added later.
set -euo pipefail

ROOT="nixxa-harness"
CONTRACTS="nixxa-contracts"

violations=$(cargo metadata --format-version 1 --no-deps | jq -r --arg root "$ROOT" --arg contracts "$CONTRACTS" '
  [.packages[].name] as $members
  | .packages[]
  | .name as $pkg
  | select($pkg != $root)
  | .dependencies[]
  | .name as $dep
  | select($members | index($dep))
  | select($dep != $pkg)
  | select($pkg == $contracts or $dep != $contracts)
  | "  \($pkg) -> \($dep) (\(.kind // "normal"))"
')

if [[ -n "$violations" ]]; then
  echo "Dependency rule violated — modules may depend only on $CONTRACTS (only $ROOT may wire modules together):"
  echo "$violations"
  exit 1
fi
echo "Dependency rule OK."
