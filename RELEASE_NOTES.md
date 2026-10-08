## What's new

- **Place names** -- districts, buildings and landmarks are lettered onto the
  maps in an old printer's face: Night Harbor's gates, temples and inns,
  Ail'Vorith's palace and arena, Faelindral's terraces and more, across eight
  zones. They are part of the map, not markers; **Place names on map** hides
  them.
- **Starter markers for most zones** -- crafting stations, banks, altars, ore
  and fishing spots, teleporters and cave entrances, taken from the game's own
  files and checked against each zone's real ground. Each zone's set arrives
  once as its *starter pack*; untick **imported** to hide them, and any you
  delete stay deleted.
- **One continuous page** -- the map no longer sits on a different sheet of
  paper: its background runs on past its edge and turns with it, so there is
  no visible square when you rotate.
- **Maps fade out where they are cut off** instead of stopping at a ruled
  line -- Underdocks' lake and tunnels, for one.
- **The painted style is repainted like an atlas**: softer earth tones, gentle
  shorelines, natural edges between grass, dirt and sand, ink on walls and
  cliffs, hand-laid paving, arching palms, canvas awnings.
- **Lamplight by moonlight** -- unlit ground is cool and moonlit rather than
  muddy, and coloured lights glow rather than paint.
- The legend lists only the markers actually shown.
- Removed three zone lines that sat far off their maps.

The new paint style and the softened edges appear once a map is rebuilt:
open **Maps...** and press **Rebuild maps**, and **Repaint this map** for
painted ones.

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
