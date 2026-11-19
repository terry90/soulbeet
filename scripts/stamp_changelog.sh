#!/bin/bash

set -e

VERSION=$1
DATE=$2
FILE=$3

if [ -z "$VERSION" ] || [ -z "$DATE" ] || [ -z "$FILE" ]; then
    echo "Usage: $0 <version> <YYYY-MM-DD> <changelog file>"
    exit 1
fi

if [ ! -f "$FILE" ]; then
    echo "$FILE not found"
    exit 1
fi

# the release notes written since the last release become this release's
if grep -q '^## Unreleased$' "$FILE"; then
    sed -i "s/^## Unreleased$/## $VERSION - $DATE/" "$FILE"
    echo "Updated $FILE"
else
    echo "No Unreleased section in $FILE, nothing to stamp"
fi
