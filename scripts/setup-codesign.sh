#!/usr/bin/env bash
# Detect or prompt for a codesigning identity and save it to .codesign-identity
# for use by the Makefile.
#
# To create a certificate, see:
# https://support.apple.com/en-au/guide/keychain-access/kyca8916/mac
#
# Choose "Keychain Access > Certificate Assistant > Create a Certificate",
# set Identity Type to "Self Signed Root" and Certificate Type to "Code Signing".

set -euo pipefail

IDENTITY_FILE=".codesign-identity"

if [[ -f "$IDENTITY_FILE" ]]; then
    existing=$(cat "$IDENTITY_FILE")
    echo "Current codesigning identity: $existing"
    read -rp "Keep this identity? [Y/n] " answer
    if [[ "${answer,,}" != "n" ]]; then
        echo "Keeping: $existing"
        exit 0
    fi
fi

# List available codesigning identities
echo ""
echo "Available codesigning identities:"
echo ""
identities=$(security find-identity -v -p codesigning 2>/dev/null | grep -v "^$" | grep -v "valid identities found" || true)

if [[ -z "$identities" ]]; then
    echo "  (none found)"
    echo ""
    echo "You need a codesigning certificate. To create one:"
    echo "  1. Open Keychain Access"
    echo "  2. Keychain Access > Certificate Assistant > Create a Certificate"
    echo "  3. Set Identity Type to 'Self Signed Root'"
    echo "  4. Set Certificate Type to 'Code Signing'"
    echo ""
    echo "See: https://support.apple.com/en-au/guide/keychain-access/kyca8916/mac"
    echo ""
    read -rp "Enter your certificate name (or '-' for ad-hoc signing): " identity
else
    echo "$identities"
    echo ""
    read -rp "Enter the certificate name from the list above: " identity
fi

if [[ -z "$identity" ]]; then
    echo "No identity provided. Aborting."
    exit 1
fi

echo "$identity" > "$IDENTITY_FILE"
echo ""
echo "Saved to $IDENTITY_FILE: $identity"
