## What's new

- **Updates itself** -- when a new version is out, a banner offers to install
  it. It downloads the new version, checks the maintainer's signature, and
  swaps it in; restart to use it. This is the last version you need to
  download by hand. Untick **Check for updates on launch** in the sidebar if
  you would rather it never asked GitHub.
- **Overlay mode** -- press **O** (or **Overlay** in the sidebar) to put the map
  on top of the game, borderless and see-through. **Ctrl+Shift+M** in the game
  switches between playing (clicks go through to the game; the map can fade or
  vanish) and using the map.
- **Drop a marker in two clicks** -- right-click the map for a ring of marker
  kinds and click one. The middle of the ring opens the full dialog as before.
- **Search every zone** -- the find box now lists matches in other zones too;
  click one to go there.
- **Subscribe to marker packs** -- paste a pack's link under **Share… →
  Subscriptions** and it is checked for new markers every time the app starts.
- **Faster first run** -- the zone you are standing in is built first and opens
  in about a minute; the rest build behind it.
- **Shortcuts** -- press **?** or **F1** for every key and mouse action.

## Install

A single self-contained executable. No installer, no runtime, and no map data
to download &mdash; it builds the maps itself from your own copy of the game.

**Linux**
```
tar xzf mnm-cartographer-*-linux-x86_64.tar.gz
./mnm-cartographer/mnm-cartographer
```

**Windows** &mdash; unzip and run `mnm-cartographer.exe`. Windows will warn
that the publisher is unknown, because the binary is not code-signed:
**More info -> Run anyway**.

On first launch press **Generate maps**. It finds your game install, reads the
zones from the game's own files and renders them. Expect a few minutes; maps
are written to your user data directory, not next to the executable, and come
to about 150 MB.

Put the executable wherever you like &mdash; it does not need to live in the
game folder.

If something looks wrong, run `mnm-cartographer --check` and include the
output in a report.

Verify a download against `SHA256SUMS.txt`. Its signature,
`SHA256SUMS.txt.minisig`, is checked with
[minisign](https://jedisct1.github.io/minisign/):

```
minisign -Vm SHA256SUMS.txt -P RWSQKY1DixXTwTuqmzG0lTEUsrR10OYr3mYrUoZ3sc9eDjnI09cJJCZA
```
