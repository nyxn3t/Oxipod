#!/usr/bin/env bash
#
# Publishes a release of Oxipod to its own Flatpak repository, which lives on
# GitHub Pages (the gh-pages branch). People add that repository once and then
# get updates through their software center or `flatpak update`.
#
#   flatpak/publish-pages.sh 0.6.0
#   flatpak/publish-pages.sh --site     (only rewrites the pages, no build, no signing)
#
# What it does: builds the release from its git tag (the tag has to be on
# GitHub already), signs the build with your key, adds it to the repository
# in your checkout of the gh-pages branch, and writes the pages next to it.
# It does not commit or push anything: look at the result, then do that.
#
# It builds with plain flatpak-builder and not with Flathub's flathub-build,
# because flathub-build rewrites the screenshot addresses in the app's store
# listing to Flathub's own server, which doesn't have Oxipod's pictures.
#
# You need:
#   OXIPOD_KEY    the fingerprint of your signing key (a GnuPG key)
#   OXIPOD_PAGES  a checkout of the gh-pages branch, default ~/Projects/oxipod-pages
#   OXIPOD_BUILD  a folder to build in, default ~/Projects/oxipod-flathub. It has
#                 to be inside your home folder: the Flatpak builder can't see /tmp.
#   OXIPOD_URL    where the pages will be, default https://nyxn3t.github.io/Oxipod
#   GNUPGHOME     only if your key isn't in the usual place

set -euo pipefail

mode=release
if [ "${1:-}" = "--site" ]; then
  mode=site
  version=
else
  version=${1:?usage: publish-pages.sh VERSION   (for example 0.6.0), or --site}
fi
tag=v$version
key=${OXIPOD_KEY:?set OXIPOD_KEY to the fingerprint of your signing key}
pages=${OXIPOD_PAGES:-$HOME/Projects/oxipod-pages}
build=${OXIPOD_BUILD:-$HOME/Projects/oxipod-flathub}
url=${OXIPOD_URL:-https://nyxn3t.github.io/Oxipod}
gnupg=${GNUPGHOME:-$HOME/.gnupg}

app=io.github.nyxn3t.Oxipod
ref=app/$app/x86_64/stable
here=$(cd "$(dirname "$0")" && pwd)
source_repo=$(dirname "$here")

say() { printf '\n== %s\n' "$*"; }

write_site() {
  say "Writing the pages"
  local public_key fingerprint spaced
  public_key=$(gpg --homedir "$gnupg" --export "$key" | base64 -w0)
  fingerprint=$(gpg --homedir "$gnupg" --list-keys --with-colons "$key" | awk -F: '/^fpr/ {print $10; exit}')
  # The same fingerprint in groups of four, which is easier to compare by eye.
  spaced=$(echo "$fingerprint" | sed 's/.\{4\}/& /g; s/ $//')
  touch "$pages/.nojekyll"

  cat > "$pages/oxipod.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Oxipod
Url=$url/repo/
Homepage=https://github.com/nyxn3t/Oxipod
Comment=Put music on your old iPod
Description=Oxipod's own Flatpak repository: new releases show up in your software center
GPGKey=$public_key
EOF

  cat > "$pages/oxipod.flatpakref" <<EOF
[Flatpak Ref]
Title=Oxipod
Name=$app
Branch=stable
Url=$url/repo/
SuggestRemoteName=oxipod
IsRuntime=false
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
Homepage=https://github.com/nyxn3t/Oxipod
Comment=Put music on your old iPod
GPGKey=$public_key
EOF

  cat > "$pages/index.html" <<EOF
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>Oxipod, the Flatpak repository</title>
<style>
  body { font: 16px/1.5 system-ui, sans-serif; max-width: 42rem; margin: 2rem auto; padding: 0 1rem; }
  pre { background: rgba(127, 127, 127, 0.15); padding: 0.75rem 1rem; border-radius: 0.5rem; overflow-x: auto; }
  code { font-family: ui-monospace, monospace; }
</style>
</head>
<body>
<h1>Oxipod</h1>
<p>Put music on your old iPod from Linux. This is Oxipod's own Flatpak repository: add it once, and new
releases show up in your software center and with <code>flatpak update</code>. You need
<a href="https://flatpak.org/setup/">Flatpak</a> on your system.</p>

<pre><code>flatpak remote-add --user --if-not-exists oxipod $url/oxipod.flatpakrepo
flatpak install --user oxipod $app</code></pre>

<p>Or open <a href="oxipod.flatpakref">oxipod.flatpakref</a> with your software center.</p>

<p>This is not Flathub. Adding the repository means you trust me, nyxn3t, the author of Oxipod, to ship
updates. They are signed with this key, which Flatpak checks for you:</p>
<p><code>$spaced</code></p>
<p>The same fingerprint is in <a href="https://github.com/nyxn3t/Oxipod#installing">the README on GitHub</a>,
so you can compare the two.</p>

<p>Oxipod should work with the iPod classic, the mini and the nano up to the 4th generation, and so far it
has only been tried on a nano 3rd gen (<a href="https://github.com/nyxn3t/Oxipod#which-ipods-work">more
about which iPods work</a>). <a href="https://github.com/nyxn3t/Oxipod">Oxipod on GitHub</a> has the source,
the releases with a plain <code>.flatpak</code> file, and a place to say how it works on your iPod.</p>
</body>
</html>
EOF
}


if [ "$mode" = site ]; then
  [ -d "$pages" ] || { echo "No $pages"; exit 1; }
  write_site
  echo "Look it over, then: cd $pages && git add -A && git commit -m 'Update the pages' && git push origin gh-pages"
  exit 0
fi

say "Checking $tag"
commit=$(git -C "$source_repo" rev-parse "$tag^{commit}")
# The build fetches the tag from GitHub, so it has to be there, and the same.
online=$(git -C "$source_repo" ls-remote --tags origin "refs/tags/$tag^{}" | cut -f1)
[ -n "$online" ] || online=$(git -C "$source_repo" ls-remote --tags origin "refs/tags/$tag" | cut -f1)
[ "$online" = "$commit" ] || { echo "The tag $tag is not on GitHub, or points somewhere else there."; exit 1; }
echo "$tag is $commit"
gpg --homedir "$gnupg" --list-secret-keys "$key" >/dev/null || { echo "No secret key $key in $gnupg"; exit 1; }
[ -d "$pages" ] || { echo "No $pages: make a checkout of the gh-pages branch there, or set OXIPOD_PAGES."; exit 1; }

say "Building $version (this is quick when libgpod and ffmpeg are cached)"
mkdir -p "$build/pages-build"
cp "$here/cargo-sources.json" "$build/"
rm -rf "$build/patches" && cp -r "$here/patches" "$build/patches"
# The manifest in the repo builds from the local folder; this one builds from the tag.
python3 - "$here/$app.yml" "$build/manifest-$version.yml" "$tag" "$commit" <<'PY'
import re, sys
source, target, tag, commit = sys.argv[1:]
text = open(source).read()
local = re.search(r"      # For local builds.*?\n(?=      - cargo-sources\.json)", text, re.S)
assert local, "the local source block was not found in the manifest"
tagged = (
    "      - type: git\n"
    "        url: https://github.com/nyxn3t/Oxipod.git\n"
    f"        tag: {tag}\n"
    f"        commit: {commit}\n"
)
open(target, "w").write(text.replace(local.group(0), tagged))
PY
(
  cd "$build"
  rm -rf "pages-build/repo-$version" "pages-build/builddir-$version"
  flatpak run org.flatpak.Builder --force-clean --user --install-deps-from=flathub --ccache \
    --default-branch=stable --repo="pages-build/repo-$version" "pages-build/builddir-$version" "manifest-$version.yml"
  rm -rf "pages-build/builddir-$version" "manifest-$version.yml"
)

say "Adding it to the repository in $pages (the signing may ask for your passphrase)"
if [ ! -d "$pages/repo/objects" ]; then
  mkdir -p "$pages/repo"
  flatpak run --command=ostree org.flatpak.Builder --repo="$pages/repo" init --mode=archive-z2
fi
flatpak build-commit-from --src-repo="$build/pages-build/repo-$version" --src-ref="$ref" \
  --gpg-sign="$key" --gpg-homedir="$gnupg" --subject="Oxipod $version" "$pages/repo" "$ref"
flatpak build-update-repo --generate-static-deltas --gpg-sign="$key" --gpg-homedir="$gnupg" \
  --title="Oxipod" --default-branch=stable --prune "$pages/repo"

write_site

say "Done: $version is in $pages"
echo "Look it over, then:"
echo "  cd $pages && git add -A && git commit -m 'Oxipod $version' && git push origin gh-pages"
