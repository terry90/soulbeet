#!/bin/bash

set -e

BUMP_TYPE=$1

if [ -z "$BUMP_TYPE" ]; then
    echo "Usage: $0 <patch|minor|major|X.Y.Z[-prerelease]>"
    exit 1
fi

# get current version
get_version() {
    grep '^version =' api/Cargo.toml | head -n 1 | cut -d '"' -f 2
}

CURRENT_VERSION=$(get_version)
CURRENT_CORE="${CURRENT_VERSION%%-*}"
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_CORE"

if [ "$BUMP_TYPE" == "major" ]; then
    MAJOR=$((MAJOR + 1))
    MINOR=0
    PATCH=0
    NEW_VERSION="$MAJOR.$MINOR.$PATCH"
elif [ "$BUMP_TYPE" == "minor" ]; then
    MINOR=$((MINOR + 1))
    PATCH=0
    NEW_VERSION="$MAJOR.$MINOR.$PATCH"
elif [ "$BUMP_TYPE" == "patch" ]; then
    PATCH=$((PATCH + 1))
    NEW_VERSION="$MAJOR.$MINOR.$PATCH"
elif [[ "$BUMP_TYPE" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    NEW_VERSION="${BUMP_TYPE#v}"
else
    echo "Invalid bump type. Use patch, minor, major, or an explicit semver version."
    exit 1
fi

echo "Bumping version from $CURRENT_VERSION to $NEW_VERSION"

# we don't care about atomic versioning, set everything to the same version
FILES=(
    "desktop/Cargo.toml"
    "mobile/Cargo.toml"
    "api/Cargo.toml"
    "lib/soulbeet/Cargo.toml"
    "lib/shared/Cargo.toml"
    "web/Cargo.toml"
    "ui/Cargo.toml"
)

for FILE in "${FILES[@]}"; do
    if [ -f "$FILE" ]; then
        sed -i "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" "$FILE"
        echo "Updated $FILE"
    else
        echo "Warning: $FILE not found"
    fi
done

# Cargo.lock records the workspace crates' versions too; refresh only those
cargo update --workspace

# the app shows CHANGELOG.md in "What's new"; date it in UTC like the release
# CHANGELOG.md headings use plain major.minor.patch (prerelease suffixes are stripped)
"$(dirname "$0")/stamp_changelog.sh" "${NEW_VERSION%%-*}" "$(date -u +%Y-%m-%d)" CHANGELOG.md

# for GitHub Actions
if [ -n "$GITHUB_OUTPUT" ]; then
    echo "new_version=$NEW_VERSION" >> $GITHUB_OUTPUT
fi