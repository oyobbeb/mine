# Icon

`mine.svg` is the source: a small ember that never goes out, on the app's charcoal. `mine.icns` is generated from it for the app bundle (`scripts/bundle-app.sh` copies it into `Contents/Resources`).

To regenerate `mine.icns` after editing the SVG (macOS, needs Google Chrome for a transparent render):

```sh
cd assets/icon
tmp=$(mktemp -d)
printf '<style>html,body{margin:0;background:transparent}img{display:block;width:1024px}</style><img src="%s">' \
  "file://$PWD/mine.svg" > "$tmp/render.html"
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --disable-gpu --hide-scrollbars \
  --default-background-color=00000000 --window-size=1024,1024 --screenshot="$tmp/1024.png" "file://$tmp/render.html"
mkdir "$tmp/mine.iconset"
for spec in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x 256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
  sips -z "${spec%%:*}" "${spec%%:*}" "$tmp/1024.png" --out "$tmp/mine.iconset/icon_${spec#*:}.png" >/dev/null
done
iconutil -c icns "$tmp/mine.iconset" -o mine.icns
```
