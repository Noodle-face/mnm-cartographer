## What's new

- **North is where players expect it** -- maps were a quarter-turn out: what
  the compass called north was west. North now matches the community's world
  map of Calafrey and Szurr, worked out from where each zone's scenery of its
  neighbours lies. Every zone opens north-up; any turn you had saved is reset.
- **Rebuild for upright symbols** -- maps built by earlier versions show palms,
  tents and campfires on their sides. The sidebar says so and offers
  **Rebuild maps** (about five minutes). Painted zones also need **Repaint this
  map**.

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
