#!/usr/bin/env bash
# provenance/check.sh — verify trillium/herdr fork extensions survived an upstream merge.
#
# The fork adds the fleet federation module (phases N0–N1d plus status/CLI work)
# on top of upstream herdr. A rebase or merge that drops any of it should fail
# loudly rather than silently. Every check here corresponds to an entry in
# provenance/register.toml — keep the two in sync.
#
# Run from anywhere in the repo: bash provenance/check.sh
#            or via the toolchain: just provenance-check
set -euo pipefail

PASS=0
FAIL=0
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

check_dir() {
    local id="$1" path="$2"
    if [ -e "$path" ]; then
        printf "  OK  %s\n" "$id"
        PASS=$((PASS + 1))
    else
        printf " FAIL %s — path not found: %s\n" "$id" "$path"
        FAIL=$((FAIL + 1))
    fi
}

check_grep() {
    local id="$1" file="$2" pattern="$3"
    if [ ! -f "$file" ]; then
        printf " FAIL %s — file not found: %s\n" "$id" "$file"
        FAIL=$((FAIL + 1))
        return
    fi
    if grep -qF -- "$pattern" "$file"; then
        printf "  OK  %s\n" "$id"
        PASS=$((PASS + 1))
    else
        printf " FAIL %s — pattern '%s' not found in %s\n" "$id" "$pattern" "$file"
        FAIL=$((FAIL + 1))
    fi
}

echo "herdr-oss fork provenance check"
echo "================================"

check_dir  "federation-module"                "src/federation"
check_grep "federation-origin-key"            "src/federation/origin.rs"      "pub struct OriginKey"
check_grep "federation-connection-target"     "src/federation/origin.rs"      "pub enum ConnectionTarget"
check_grep "federation-registry"              "src/federation/registry.rs"    "pub struct FederationRegistry"
check_grep "federation-reconcile"             "src/federation/registry.rs"    "pub fn reconcile"
check_grep "federation-ingest"                "src/federation/ingest.rs"      "pub struct ForeignRows"
check_grep "federation-namespace"             "src/federation/namespace.rs"   "namespace_terminal_id"
check_grep "federation-namespace-prefix"      "src/federation/namespace.rs"   "FED_PREFIX"
check_grep "federation-poll"                  "src/federation/poll.rs"        "collect_foreign_rows"
check_grep "federation-relay"                 "src/federation/relay.rs"       "pub fn send_input_to_foreign_pane"
check_grep "federation-config"                "src/config/model.rs"           "pub struct FederationConfig"
check_grep "federation-app-integration"       "src/app/mod.rs"                "spawn_federation_poll"
check_grep "federation-foreign-rows-channel"  "src/app/mod.rs"                "foreign_rows_tx"
check_grep "federation-input-terminal"        "src/app/input/terminal.rs"     "foreign"
check_grep "federation-status"                "src/federation/status.rs"      "pub struct FederationStatusTracker"
check_grep "federation-fleet-cli"             "src/cli/fleet.rs"              "pub(crate) fn run_fleet_command"
check_grep "federation-fleet-cli-dispatch"    "src/cli.rs"                    "fleet::run_fleet_command"

echo ""
echo "Result: $PASS passed, $FAIL failed"
echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "All fork extensions present — rebase/merge preserved them."
    exit 0
fi
echo "PROVENANCE FAILURE: $FAIL feature(s) missing — upstream merge dropped fork extensions." >&2
exit 1
