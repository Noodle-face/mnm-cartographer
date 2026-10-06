## What's new

- **Cooler map building** -- building maps now rests the CPU in short, regular
  pauses by default (**Balanced**). It takes about a fifth longer and keeps
  the CPU around 10 °C cooler: on a Ryzen 9800X3D a full rebuild peaked at
  86 °C instead of 95 °C. **CPU while building** in the Maps panel offers Full
  speed, Balanced and Cool.
- **Gentler on memory** -- how many zones build at once now follows how much
  memory is actually free, and a build waits rather than push a PC with the
  game open into running out.
- **A heads-up before building** -- the Maps panel and first-run screen now say
  what a build does to your CPU, and that building while the game is running
  may cause instability.

You are on 0.0.5 or later, so **Update now** in the banner installs this.

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
