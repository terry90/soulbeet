#!/bin/bash

set -e

BUMP_TYPE=$1

if [ -z "$BUMP_TYPE" ]; then
    echo "Usage: $0 <patch|minor|major>"
    exit 1
fi

# get current version
get_version() {
    grep '^version =' api/Cargo.toml | head -n 1 | cut -d '"' -f 2
}

CURRENT_VERSION=$(get_version)
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"

if [ "$BUMP_TYPE" == "major" ]; then
    MAJOR=$((MAJOR + 1))
    MINOR=0
    PATCH=0
elif [ "$BUMP_TYPE" == "minor" ]; then
    MINOR=$((MINOR + 1))
    PATCH=0
elif [ "$BUMP_TYPE" == "patch" ]; then
    PATCH=$((PATCH + 1))
else
    echo "Invalid bump type. Use patch, minor, or major."
    exit 1
fi

NEW_VERSION="$MAJOR.$MINOR.$PATCH"
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

# for GitHub Actions
if [ -n "$GITHUB_OUTPUT" ]; then
    echo "new_version=$NEW_VERSION" >> $GITHUB_OUTPUT
fi