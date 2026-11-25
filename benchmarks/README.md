# RX 7900 XT benchmarks

## First room

This branch builds on [the accepted-present change](https://github.com/avatarmanz/VIctory-7900XTX/pull/17) and records the accepted frames, elapsed microseconds and executed draws once per reporting interval. Build with `cargo build --locked --profile diag -p victory --features diagnostics`. Keep the log locally, run the same scene, then count the complete intervals within the measurement window.

```sh
python benchmarks/first_room.py path/to/run.log --start 150 --stop 225
```

I used a Ryzen 7 9700X, RX 7900 XT, RADV 26.3.1, Wine 11.22 and vkd3d-proton v3.1-83 with the Series X profile. Each session ran for 230 seconds on a shared desktop. I checked the on-screen scene separately from the counter.

On the first run with an empty shader cache and the fork’s default play preset, the swapchain accepted 207 presents in 73.18 s (2.83 FPS, 2,262 draws/frame). At 175 s I saw a black image. At 216 s the movement prompt was visible against black. This run did not yield a rendered first-room gameplay rate.

## Descriptor heap rescan

The CPU benchmark extracts the actual slot set from each Git revision. It checks all sixteen partitions return the same slots in the same order before timing them. Eight alternating pairs each perform eight complete rounds over a synthetic 1.2-million-slot heap. Run from the repository root with Python 3 and Rust.

```sh
git fetch https://github.com/UwUCammie/VIctory-7900XTX.git perf/avatar-mask-heap-rescan
python benchmarks/slot_rescan.py 0a43566 FETCH_HEAD
```

On the 9700X, eight rounds over 83,000 live slots took a median of 6.28 ms with per-slot filtering and 1.42 ms with word masks. At 128 live slots the medians were 0.93 and 0.78 ms. These timings cover slot enumeration only.
