#!/bin/sh
# Build, test, and copy the page into a site directory, e.g.
#   web/publish.sh ../loot/webb/warez/11-alexandrite
# meta.yml in the destination (the site's title/description) is kept.
set -e
dest="${1:?usage: web/publish.sh DEST}"
here="$(cd "$(dirname "$0")" && pwd)"
"$here/build.sh"
node "$here/test.mjs" > /dev/null
mkdir -p "$dest"
rsync -a --delete --exclude meta.yml "$here/www/" "$dest/"
# The site serves .js/.wasm/.css as immutable for a year under fixed names:
# version every internal reference so a publish is never served stale.
v="$(cat "$dest"/pkg/alx_web_bg.wasm "$dest"/*.js "$dest"/*.css | shasum | cut -c1-10)"
perl -pi -e "s/((?:href|src)=\"\.?\/?[\w.\/]+\.(?:js|css))\"/\$1?v=$v\"/g" "$dest/index.html"
perl -pi -e "s/'(\.\/[\w.\/]+\.(?:js|wasm|txt))'/'\$1?v=$v'/g" "$dest"/main.js "$dest"/worker.js
echo "published to $dest (v=$v)"
