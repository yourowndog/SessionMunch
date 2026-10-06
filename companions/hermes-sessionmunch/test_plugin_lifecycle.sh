#!/bin/bash
set -euo pipefail

echo "--- Plugin Lifecycle Test ---"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TEST_HOME="$(mktemp -d "${TMPDIR:-/tmp}/sessionmunch-hermes-lifecycle.XXXXXX")"
trap 'rm -rf -- "$TEST_HOME"' EXIT
export HERMES_HOME="$TEST_HOME"
PLUGIN_DIR="$HERMES_HOME/plugins/sessionmunch"

hermes config set memory.provider sessionmunch >/dev/null

uninstalled_output="$(hermes memory status)"
grep -Fq "Provider:  sessionmunch" <<<"$uninstalled_output"
grep -Fq "Plugin:    NOT installed ✗" <<<"$uninstalled_output"
echo "PASS: Isolated home reports the plugin uninstalled"

mkdir -p "$(dirname -- "$PLUGIN_DIR")"
cp -R -- "$SCRIPT_DIR" "$PLUGIN_DIR"

installed_output="$(hermes memory status)"
grep -Fq "Provider:  sessionmunch" <<<"$installed_output"
grep -Fq "Plugin:    installed ✓" <<<"$installed_output"
grep -Fq "• sessionmunch  (API key / local) ← active" <<<"$installed_output"
echo "PASS: Isolated home reports the plugin installed and active"

rm -rf -- "$PLUGIN_DIR"

removed_output="$(hermes memory status)"
grep -Fq "Provider:  sessionmunch" <<<"$removed_output"
grep -Fq "Plugin:    NOT installed ✗" <<<"$removed_output"
if grep -Fq "• sessionmunch" <<<"$removed_output"; then
    echo "FAIL: Removed plugin still appears in installed plugins"
    exit 1
fi
echo "PASS: Isolated home reports the plugin removed"

echo "PASS: Tests completed."
