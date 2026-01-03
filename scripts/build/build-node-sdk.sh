#!/bin/bash
set -e

# Build Node.js SDK with napi-rs
# Usage: build-node-sdk.sh [--profile debug|release]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
NODE_SDK_DIR="$PROJECT_ROOT/sdks/node"
RUNTIME_DIR="$PROJECT_ROOT/target/boxlite-runtime"
OUTPUT_DIR="$NODE_SDK_DIR/packages"

# Parse arguments
PROFILE="release"
while [[ $# -gt 0 ]]; do
    case $1 in
        --profile)
            PROFILE="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

echo "📦 Building Node.js SDK ($PROFILE mode)..."

# Install dependencies
cd "$NODE_SDK_DIR"
npm install --silent

# Build native addon
echo "🔨 Building native addon with napi-rs..."
if [ "$PROFILE" = "release" ]; then
    npx napi build --platform --release
else
    npx napi build --platform
fi

# Build TypeScript
echo "📦 Building TypeScript..."
npm run build

# Find the native module (napi-rs outputs index.{platform}-{arch}.node by default)
if [ "$(uname)" = "Darwin" ]; then
    NATIVE_MODULE=$(find "$NODE_SDK_DIR" -maxdepth 1 -name "index.darwin-*.node" -type f | head -1)
else
    NATIVE_MODULE=$(find "$NODE_SDK_DIR" -maxdepth 1 -name "index.linux-*.node" -type f | head -1)
fi

if [ -z "$NATIVE_MODULE" ]; then
    echo "❌ Native module not found"
    exit 1
fi

# Add rpath
echo "🔗 Adding rpath to native module..."
if [ "$(uname)" = "Darwin" ]; then
    install_name_tool -add_rpath @loader_path/runtime "$NATIVE_MODULE" 2>/dev/null || true
else
    patchelf --set-rpath '$ORIGIN/runtime' "$NATIVE_MODULE" 2>/dev/null || true
fi

# Determine platform directory (extract darwin-arm64/linux-x64/etc from index.{platform}.node)
PLATFORM_DIR=$(basename "$NATIVE_MODULE" | sed 's/index\.\(.*\)\.node/\1/')
PKG_DIR="$NODE_SDK_DIR/npm/$PLATFORM_DIR"

# Copy native module to platform package
echo "📦 Copying native module to platform package..."
mkdir -p "$PKG_DIR"
cp "$NATIVE_MODULE" "$PKG_DIR/"

# Generate platform package.json
NATIVE_BASENAME=$(basename "$NATIVE_MODULE")
if [[ "$PLATFORM_DIR" == darwin-* ]]; then
    PKG_OS="darwin"
elif [[ "$PLATFORM_DIR" == linux-* ]]; then
    PKG_OS="linux"
else
    PKG_OS="unknown"
fi

if [[ "$PLATFORM_DIR" == *-arm64 ]]; then
    PKG_CPU="arm64"
elif [[ "$PLATFORM_DIR" == *-x64 ]]; then
    PKG_CPU="x64"
else
    PKG_CPU="unknown"
fi

cat > "$PKG_DIR/package.json" << EOF
{
  "name": "@boxlite/boxlite-$PLATFORM_DIR",
  "version": "0.1.0",
  "os": ["$PKG_OS"],
  "cpu": ["$PKG_CPU"],
  "main": "$NATIVE_BASENAME",
  "files": [
    "$NATIVE_BASENAME",
    "runtime"
  ],
  "description": "BoxLite native bindings for $PLATFORM_DIR",
  "license": "Apache-2.0",
  "repository": {
    "type": "git",
    "url": "https://github.com/boxlite-labs/boxlite.git",
    "directory": "sdks/node"
  },
  "engines": {
    "node": ">=18.0.0"
  }
}
EOF

# Copy runtime to platform package
echo "📦 Copying runtime to platform package..."
rm -rf "$PKG_DIR/runtime"
cp -a "$RUNTIME_DIR" "$PKG_DIR/runtime"

# Create output directory and tarballs
echo "📦 Creating tarballs..."
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

# Pack main package
cd "$NODE_SDK_DIR"
npm pack --pack-destination "$OUTPUT_DIR"

# Pack platform package
cd "$PKG_DIR"
npm pack --pack-destination "$OUTPUT_DIR"

echo ""
echo "✅ Node.js SDK built successfully"
echo ""
echo "   Output: sdks/node/packages/"
ls -1h "$OUTPUT_DIR"
echo ""
echo "   Install: npm install sdks/node/packages/boxlite-boxlite-0.1.0.tgz sdks/node/packages/boxlite-boxlite-$PLATFORM_DIR-0.1.0.tgz"
