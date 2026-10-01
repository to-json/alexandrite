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
echo "published to $dest"
