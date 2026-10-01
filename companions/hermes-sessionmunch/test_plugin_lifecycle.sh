#!/bin/bash
set -euo pipefail

echo "--- Plugin Lifecycle Test ---"

PLUGIN_DIR="$HOME/.hermes/profiles/builder-b/plugins/sessionmunch"

# 1. Clean/Uninstall by moving away
if [ -d "$PLUGIN_DIR" ]; then
    mv "$PLUGIN_DIR" "/tmp/sessionmunch_test_trash_$(date +%s)_$RANDOM"
fi

# 2. Check uninstalled
if hermes memory status | grep -q "NOT installed"; then
    echo "PASS: Uninstalled correctly"
else
    echo "FAIL: Not uninstalled"
    exit 1
fi

# 3. Install
cp -r . "$PLUGIN_DIR"

# 4. Check installed
if hermes memory status | grep -q "installed ✓"; then
    echo "PASS: Installed correctly"
else
    echo "FAIL: Not installed"
    exit 1
fi

echo "PASS: Tests completed."
