#!/usr/bin/env bash
# Baut ein unsigniertes rk40.app Bundle aus dem Release-Binary.
# Aufruf (nach cargo build --release):
#   ./scripts/package-macos-app.sh
# Optional:
#   BINARY=… OUT_DIR=… APP_NAME=rk40 ./scripts/package-macos-app.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="${BINARY:-$ROOT/target/release/rk40}"
OUT_DIR="${OUT_DIR:-$ROOT/dist}"
APP_NAME="${APP_NAME:-rk40}"
BUNDLE="$OUT_DIR/${APP_NAME}.app"
PORT="${PORT:-8040}"

if [[ ! -f "$BINARY" ]]; then
  echo "Binary nicht gefunden: $BINARY" >&2
  echo "Zuerst: cargo build --release" >&2
  exit 1
fi

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

# Launch-Wrapper: startet den Server und öffnet den Browser
cat > "$BUNDLE/Contents/MacOS/${APP_NAME}" <<EOF
#!/bin/bash
DIR="\$(cd "\$(dirname "\$0")" && pwd)"
BIN="\$DIR/${APP_NAME}-bin"
URL="http://localhost:${PORT}"

"\$BIN" --port ${PORT} &
PID=\$!

cleanup() {
  kill "\$PID" 2>/dev/null || true
  wait "\$PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Warten bis der Port antwortet (max. ~15 s)
for _ in \$(seq 1 30); do
  if curl -sf -o /dev/null "\$URL" 2>/dev/null; then
    break
  fi
  if ! kill -0 "\$PID" 2>/dev/null; then
    echo "rk40 ist unerwartet beendet." >&2
    exit 1
  fi
  sleep 0.5
done

open "\$URL"
wait "\$PID"
EOF
chmod +x "$BUNDLE/Contents/MacOS/${APP_NAME}"

cp "$BINARY" "$BUNDLE/Contents/MacOS/${APP_NAME}-bin"
chmod +x "$BUNDLE/Contents/MacOS/${APP_NAME}-bin"

cat > "$BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>${APP_NAME}</string>
  <key>CFBundleIdentifier</key>
  <string>com.corelgott.rk40</string>
  <key>CFBundleName</key>
  <string>rk40</string>
  <key>CFBundleDisplayName</key>
  <string>rk40</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

echo "Erzeugt: $BUNDLE"
