## What's new

- **Marker icons** — every marker type now has its own icon (tent, crown, leaf,
  coin, arrow, !, skull, page) on a badge of the same size, a little larger
  than before and easier to tell apart. The legend and tooltips use them too.
- **Painted style follows the ground** — the experimental painted maps now
  colour each surface by what it is made of: sand, rock, grass, dirt, mud,
  snow, lava, water, wooden planks and stone floors. Forest zones come out
  green, cities show their paving, and cliffs show as rock.

Already painted a zone with the experimental style? Press **Repaint this map**
to see the new colours. Inked maps are unchanged; no rebuild needed.

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

Verify a download against `SHA256SUMS.txt`.
