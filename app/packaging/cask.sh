#!/bin/sh
# Prints the Homebrew cask for DinoCopy.
#
#   app/packaging/cask.sh <version> <sha256 of DinoCopy.dmg>
#
# .github/workflows/build-app.yml fills it in for the built disk image, and
# publish-app.yml commits it to Casks/dinocopy.rb in oberbichler/homebrew-tap.
set -eu

if [ $# -ne 2 ]; then
  echo "usage: $0 <version> <sha256>" >&2
  exit 2
fi
version=$1
sha256=$2

case $version in
  *[!0-9A-Za-z.-]* | "") echo "invalid version: $version" >&2; exit 2 ;;
esac
if ! printf '%s' "$sha256" | grep -Eq '^[0-9a-f]{64}$'; then
  echo "invalid sha256: $sha256" >&2
  exit 2
fi

cat <<EOF
cask "dinocopy" do
  version "$version"
  sha256 "$sha256"

  url "https://github.com/oberbichler/dino-copy/releases/download/v#{version}/DinoCopy.dmg"
  name "DinoCopy"
  desc "Mirror one disk onto another"
  homepage "https://github.com/oberbichler/dino-copy"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on :macos

  app "DinoCopy.app"

  # DinoCopy is signed ad hoc, not notarized: without this Gatekeeper refuses the first start.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/DinoCopy.app"]
  end

  uninstall quit: "com.oberbichler.dino-copy"

  zap trash: [
    "~/Library/Caches/com.oberbichler.dino-copy",
    "~/Library/Saved Application State/com.oberbichler.dino-copy.savedState",
    "~/Library/WebKit/com.oberbichler.dino-copy",
  ]
end
EOF
