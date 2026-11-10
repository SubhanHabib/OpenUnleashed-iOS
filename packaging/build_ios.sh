#!/bin/sh
# iOS / iPadOS build -> target/dist/<APP_ID>-ios-<version>.ipa (a device build) or target/ios/<APP_ID>.app (simulator).
#
# The app carries no game data. On the first start it asks for the player's own copy of the game: the installed
# folder or a .zip of it, picked in Files. It can also be dropped into the app's folder in Files
# (crates/open-unleashed/src/ios/).
#
#   build_ios.sh                    a device build, signed for development: IOS_TEAM=<your team ID> is required.
#                                   Xcode's automatic signing makes the provisioning profile (Xcode must be signed in
#                                   to that team, and the device registered or connected).
#   build_ios.sh --sim              a simulator build (arm64), ad-hoc signed; no team needed
#   build_ios.sh --install [<id>]   also install it: on a connected device (`xcrun devicectl`; <id> = its UDID or
#                                   name, else the only one connected), or with --sim on the booted simulator
#   build_ios.sh --run              install and start it (implies --install)
#
# Environment: IOS_TEAM (device builds), IOS_BUNDLE_ID (default: packaging/brand.sh's BUNDLE_ID; a team that cannot
# register that identifier sets its own).
# Needs: the rustup targets aarch64-apple-ios / aarch64-apple-ios-sim, Xcode, and xcodegen (brew install xcodegen).
set -e
cd "$(dirname "$0")/.."
. packaging/brand.sh
SIM=0
INSTALL=0
RUN=0
DEVICE=
while [ $# -gt 0 ]; do
  case "$1" in
    --sim) SIM=1 ;;
    --install) INSTALL=1; case "$2" in ""|--*) ;; *) DEVICE=$2; shift ;; esac ;;
    --run) INSTALL=1; RUN=1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
BUNDLE=${IOS_BUNDLE_ID:-$BUNDLE_ID}
if [ "$SIM" = 1 ]; then TRIPLE=aarch64-apple-ios-sim; SDK=iphonesimulator; else TRIPLE=aarch64-apple-ios; SDK=iphoneos; fi
if [ "$SIM" = 0 ] && [ -z "$IOS_TEAM" ]; then
  echo "a device build is signed by your Apple developer team: set IOS_TEAM=<team ID> (or build --sim)" >&2
  exit 2
fi
command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }
ROOT=$(pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)

# 1. the game, a static Rust executable (the same crate as every other platform)
export IPHONEOS_DEPLOYMENT_TARGET=15.0
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=/build"
cargo build --release -p open-unleashed --target "$TRIPLE"
BIN=$TARGET_DIR/$TRIPLE/release/open-unleashed

# 2. an Xcode project around it: no sources of its own, a script phase puts the Rust executable in the bundle, and
# Xcode compiles the icon, writes the plist and signs (with a development profile for a device)
PROJ=$ROOT/target/ios/project
rm -rf "$PROJ"
mkdir -p "$PROJ/Assets.xcassets/AppIcon.appiconset"
cp packaging/brand/icon-ios-1024.png "$PROJ/Assets.xcassets/AppIcon.appiconset/icon-1024.png"
cat > "$PROJ/Assets.xcassets/Contents.json" <<'JSON'
{ "info" : { "author" : "xcode", "version" : 1 } }
JSON
cat > "$PROJ/Assets.xcassets/AppIcon.appiconset/Contents.json" <<'JSON'
{
  "images" : [ { "filename" : "icon-1024.png", "idiom" : "universal", "platform" : "ios", "size" : "1024x1024" } ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON
cat > "$PROJ/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleName</key><string>$APP_TITLE</string>
  <key>CFBundleDisplayName</key><string>$APP_TITLE</string>
  <key>CFBundleIdentifier</key><string>\$(PRODUCT_BUNDLE_IDENTIFIER)</string>
  <key>CFBundleExecutable</key><string>\$(EXECUTABLE_NAME)</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSRequiresIPhoneOS</key><true/>
  <!-- an empty launch screen: without one iOS runs the app letterboxed at an old phone's size -->
  <key>UILaunchScreen</key><dict/>
  <key>UIRequiresFullScreen</key><true/>
  <key>UIStatusBarHidden</key><true/>
  <key>UISupportedInterfaceOrientations</key>
  <array><string>UIInterfaceOrientationLandscapeLeft</string><string>UIInterfaceOrientationLandscapeRight</string></array>
  <key>UISupportedInterfaceOrientations~ipad</key>
  <array><string>UIInterfaceOrientationLandscapeLeft</string><string>UIInterfaceOrientationLandscapeRight</string></array>
  <!-- the app's Documents in the Files app ("On My iPhone / iPad > $APP_TITLE"): a game folder or .zip dropped there is
       imported at the next start; the log (Logs/) and the photo-mode stills (Photos/) are there too -->
  <key>UIFileSharingEnabled</key><true/>
  <key>LSSupportsOpeningDocumentsInPlace</key><true/>
  <!-- pads: the system's game mode (iPadOS 18) and the controller menu -->
  <key>GCSupportsControllerUserInteraction</key><true/>
  <key>GCSupportsGameMode</key><true/>
  <key>CADisableMinimumFrameDurationOnPhone</key><true/>
  <!-- network races on the local network (UDP 9442), as in the macOS build -->
  <key>NSLocalNetworkUsageDescription</key><string>$APP_TITLE looks for and joins network races on your local network (UDP port 9442).</string>
  <key>NSMicrophoneUsageDescription</key><string>$APP_TITLE sends your voice to the other players during network races, as the original game did.</string>
</dict></plist>
PLIST
cat > "$PROJ/project.yml" <<YAML
name: $APP_ID
options:
  deploymentTarget:
    iOS: "15.0"
targets:
  $APP_ID:
    type: application
    platform: iOS
    sources:
      - path: Assets.xcassets
    settings:
      base:
        PRODUCT_NAME: $APP_ID
        PRODUCT_BUNDLE_IDENTIFIER: $BUNDLE
        EXECUTABLE_NAME: $APP_ID
        INFOPLIST_FILE: Info.plist
        GENERATE_INFOPLIST_FILE: NO
        TARGETED_DEVICE_FAMILY: "1,2"
        ASSETCATALOG_COMPILER_APPICON_NAME: AppIcon
        ARCHS: arm64
        ONLY_ACTIVE_ARCH: NO
        CODE_SIGN_STYLE: Automatic
        DEVELOPMENT_TEAM: "${IOS_TEAM:-}"
        RUST_BINARY: "$BIN"
    preBuildScripts: []
    postCompileScripts:
      - name: Rust executable
        script: |
          set -e
          mkdir -p "\$TARGET_BUILD_DIR/\$WRAPPER_NAME"
          cp "\$RUST_BINARY" "\$TARGET_BUILD_DIR/\$EXECUTABLE_PATH"
          # the release profile keeps debug info for the desktop crash logs; a device app ships without it
          xcrun strip -S -x "\$TARGET_BUILD_DIR/\$EXECUTABLE_PATH"
          # PLATFORM GLUE: an app linked against the iOS 27 SDK must use the UIScene life cycle, or UIKit stops it at
          # launch; winit 0.30 makes its windows the pre-scene way. The executable is marked as built against the
          # iOS 18 SDK, the last that runs such an app, until winit adopts scenes.
          case "\$PLATFORM_NAME" in iphonesimulator) VPLAT=iossim ;; *) VPLAT=ios ;; esac
          xcrun vtool -set-build-version "\$VPLAT" 15.0 18.0 -replace -output "\$DERIVED_FILE_DIR/exe" "\$TARGET_BUILD_DIR/\$EXECUTABLE_PATH" 2>/dev/null
          mv "\$DERIVED_FILE_DIR/exe" "\$TARGET_BUILD_DIR/\$EXECUTABLE_PATH"
        inputFiles:
          - \$(RUST_BINARY)
        outputFiles:
          - \$(TARGET_BUILD_DIR)/\$(EXECUTABLE_PATH)
YAML
(cd "$PROJ" && xcodegen generate --quiet)
BUILD=$ROOT/target/ios/build
if [ "$SIM" = 1 ]; then
  SIGN="CODE_SIGN_IDENTITY=- CODE_SIGNING_REQUIRED=NO"
  PROV=
else
  SIGN=
  PROV="-allowProvisioningUpdates -allowProvisioningDeviceRegistration"
fi
# A device build for a named device builds for that device, so automatic signing registers it in the
# team's provisioning profile (with a generic destination, a profile may not list it and the install fails).
if [ "$SIM" = 0 ] && [ -n "$DEVICE" ]; then
  DEST="id=$DEVICE"
else
  DEST="generic/platform=$( [ "$SIM" = 1 ] && echo 'iOS Simulator' || echo iOS )"
fi
xcodebuild -project "$PROJ/$APP_ID.xcodeproj" -scheme "$APP_ID" -configuration Release -sdk "$SDK" \
  -destination "$DEST" \
  -derivedDataPath "$BUILD" $PROV $SIGN -quiet build
APP=$BUILD/Build/Products/Release-$SDK/$APP_ID.app
[ -x "$APP/$APP_ID" ] || { echo "the app has no executable: $APP" >&2; exit 1; }
echo "built $APP"

# 3. the .ipa (a device build): Payload/<app> in a zip
if [ "$SIM" = 0 ]; then
  DIST=$ROOT/target/dist
  mkdir -p "$DIST"
  COMMIT=${PACK_VERSION:-$(git rev-parse --short=8 HEAD 2>/dev/null || echo "$VERSION")}
  IPA=$DIST/$APP_ID-ios-$COMMIT.ipa
  STAGE=$ROOT/target/ios/ipa
  rm -rf "$STAGE" "$IPA"
  mkdir -p "$STAGE/Payload"
  ditto --norsrc --noextattr "$APP" "$STAGE/Payload/$APP_ID.app"
  (cd "$STAGE" && zip -qry "$IPA" Payload)
  rm -rf "$STAGE"
  echo "built $IPA"
fi

# 4. install / run
if [ "$INSTALL" = 1 ]; then
  if [ "$SIM" = 1 ]; then
    xcrun simctl install "${DEVICE:-booted}" "$APP"
    [ "$RUN" = 1 ] && xcrun simctl launch "${DEVICE:-booted}" "$BUNDLE"
  else
    if [ -z "$DEVICE" ]; then
      DEVICE=$(xcrun devicectl list devices 2>/dev/null | awk '/connected/ && !/unavailable/ {print $3; exit}')
    fi
    [ -n "$DEVICE" ] || { echo "no connected device (pass --install <UDID>)" >&2; exit 1; }
    xcrun devicectl device install app --device "$DEVICE" "$APP"
    [ "$RUN" = 1 ] && xcrun devicectl device process launch --device "$DEVICE" "$BUNDLE"
  fi
fi
true
