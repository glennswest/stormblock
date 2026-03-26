#!/bin/bash
# build-release.sh — Build stormblock release binary and upload to git.gt.lo
set -euo pipefail

REPO="${1:-gwest/stormblock}"
GITEA="http://git.gt.lo"

echo "=== Building stormblock release binary ==="
cd /build
cargo build --release 2>&1
strip target/release/stormblock
SIZE=$(du -h target/release/stormblock | cut -f1)
echo "Build complete: $SIZE"

echo "=== Creating release on $GITEA ==="
TAG="dev-$(date +%Y%m%d-%H%M%S)"
COMMIT=$(git rev-parse HEAD)

# Create release via API
RELEASE=$(curl -s -X POST "$GITEA/api/repos/$REPO/releases" \
    -H 'Content-Type: application/json' \
    -d "{\"tag\":\"$TAG\",\"name\":\"Dev build\",\"body\":\"Commit: $COMMIT\nSize: $SIZE\"}")

RELEASE_ID=$(echo "$RELEASE" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
if [ -z "$RELEASE_ID" ]; then
    echo "Failed to create release: $RELEASE"
    echo "Binary available at: target/release/stormblock ($SIZE)"
    exit 1
fi

echo "Release created: $TAG (id=$RELEASE_ID)"

# Upload binary as asset
echo "Uploading binary..."
curl -s -X POST "$GITEA/api/repos/$REPO/releases/$RELEASE_ID/assets" \
    -F "file=@target/release/stormblock" \
    -F "name=stormblock" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'Uploaded: {d.get(\"name\",\"?\")} ({d.get(\"size\",0)} bytes)')" 2>/dev/null

echo ""
echo "=== Release ready ==="
echo "  Tag:      $TAG"
echo "  Download: $GITEA/api/repos/$REPO/releases/$RELEASE_ID/assets/stormblock"
echo "  Or:       curl -LO $GITEA/r/$REPO/releases/download/$TAG/stormblock"
