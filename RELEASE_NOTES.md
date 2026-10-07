## What's new

- **Lamplight** -- painted maps can now be shown lit by the zone's own lamps,
  torches and glows: warm streets in Night Harbor, blue caverns and green
  grottos in Underdocks. Choose **Daylight** or **Lamplight** under *Show
  painted style*. Everything stays visible; the light colours the map, it does
  not hide it.
- **Buildings look like buildings** in the painted style: roofs in the game's
  own colours, shaded by their pitch, with eaves and shadows on the street.
  Night Harbor's palace dome is back -- it had been painted as dirt.
- **Forests** -- trees are drawn from the game's own tree objects.
- **Paint floors** -- each floor of a multi-storey zone can be painted.
- **Markers stay on their floor** -- a marker placed on a floor shows only
  there; set a marker's floor in its dialog.
- **Much faster map building** -- building every map takes about half as
  long as before (4:04 down to 2:12 at full speed on a 9800X3D), and painting
  Night Harbor 22 seconds instead of 55. The maps come out exactly the same.
- Fixed: props under a rotated group (all of Fallen Watch) were drawn in the
  wrong place, and some bones were being drawn as pine trees.

Painted maps from earlier versions keep their old look until you press
**Repaint this map**.

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
