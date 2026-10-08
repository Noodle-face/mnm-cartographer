## What's new

- **A proper map legend** -- the key in the corner is now set like a printed
  map's: each kind of marker on screen, its badge beside its name in its own
  colour, under a ruled title. No more counts.
- **Fixed: parts of some maps were faded away.** 0.0.9 faded maps out toward
  their trimmed edges, and where trimmed scenery lay close to the real zone
  it washed out real ground with it -- Underdocks lost its docks and the
  water channels beside them. The fade is gone; every map shows everything it
  did before 0.0.9, and a little more at the edges.

If you rebuilt maps with 0.0.9, open **Maps...** and press **Rebuild maps**,
and **Repaint this map** for painted ones, to get the missing parts back.

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
