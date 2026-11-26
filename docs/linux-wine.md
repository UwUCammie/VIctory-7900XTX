# VIctory under Wine on Linux: what was learned (2026-11-26)

One PC: Ryzen 9 9900X (12 cores, 24 threads), 46 GB of RAM, Radeon RX 7900 GRE 16 GB
(RADV, Mesa 26.3.1), Arch Linux, kernel 7.3.6 (CachyOS), GNOME on Wayland with Xwayland.
Wine is Proton Experimental's (`experimental-11.0-20261112`, Wine 11.0, ntsync on), with
vkd3d-proton 3.2.0 and DXVK's dxgi copied into the prefix, started by `scripts/play.sh`.
Series X profile, ray tracing off, diagnostics build.

Every figure is of one run of `measure.py` (see Tools; `scripts/play.sh --start`, 200 s, the
title's first scene from about second 50 on), read over seconds 100 to 200 with
`frame-stats.py`: medians of the report lines. Scenes are not the same from run to
run; read differences under about 5 % as nothing. Logs are in `~/Games/victory-logs/`.

## Where it stands

| Run | What | Frame (median) | fps | The card busy |
|---|---|---|---|---|
| 9 | before (a9400a6) | 441 ms (292 to 845) | 2.3 | 12 % |
| 20 | this tree, `VICTORY_WINE_MAP=0 VICTORY_PIPELINE_WATCH=0` | 359 ms (299 to 1,627) | 2.8 | 12 % |
| 11 | pages shown with the host's `mmap` (`VICTORY_WINE_MAP`, on under Wine) | 287 ms (243 to 314) | 3.5 | 18 % |
| 21 | workers watched, pages shown as views (`VICTORY_WINE_MAP=0`) | 279 ms (56 to 759) | 3.6 | 15 % |
| 19 | both (the default under Wine now) | **47.9 ms** (40.5 to 71.6) | **20.9** | 90 % |
| 26 | both, and renderer patches 2, 5, 6 and 7 (`VICTORY_PORT_DESCRIPTORS`, `_CBV`, `_GUARD`, `_JOURNAL`) | 48.2 ms (38.1 to 59.6) | 20.7 | 91 % |
| 27 | both, and `VICTORY_OUTPUT=1920x1080` | **39.2 ms** (37.2 to 55.0) | **25.5** | 78 % |

The README's figure on Windows with an RX 7900 XTX is 45 ms. What sets the frame under Wine
now is what sets it there: the card (90 % busy) and the thread that submits (47 ms a frame).

There were two causes, one behind the other: with either one left in, the frame is about
280 ms (runs 11 and 21). Neither was the cost of `GetWriteWatch`.

## Cause 1: every view mapped or unmapped walks a list in Wine's server

The title shows its physical pages one 64 KiB view at a time (`XMemMapPhysicalPagesScatter`
and the GPU's page-mapping copies): 2,000 to 5,500 `MapViewOfFile3` and `UnmapViewOfFile2`
a second in the first scene, with more than 50,000 views there (53,000 mappings in
`/proc/<pid>/maps`, 8 GB of shared memory).

Wine's server keeps a process's views in a linked list (`server/mapping.c`). `map_view`
walks all of it (`is_valid_view_addr` looks for an overlap), `unmap_view` walks it to the
view (`find_mapped_view`; new views are at the end). The calling thread holds ntdll's
`virtual_mutex` across the request (`virtual_map_section`, `unmap_view_of_section` in
`dlls/ntdll/unix/virtual.c`), and that lock is the one every `NtProtectVirtualMemory`,
`NtQueryVirtualMemory`, `NtGetWriteWatch` and every fault (`virtual_handle_fault`) of the
process takes too.

Measured:

- `tests/fixtures/wine-bench` (`bench1-proton.txt`): a map and unmap of one view takes 7 us
  with no other view there, 49 us with 10,000, 132 with 30,000, 253 with 60,000, 498 with
  120,000. Beside a thread that maps and unmaps, with 60,000 views there, a write fault
  opened by a vectored handler takes 195 us on another thread (2.4 alone), a protection
  change 48 us (1.5 alone), a refused `GetWriteWatch` 18 us (0.2 alone).
- In the title (run 10, `VICTORY_DSTORAGE_LOG`, seconds 70 on): 1,061 maps a second of 418 us
  and 1,017 unmaps of 319 us, 768 ms of every second in those calls on one thread.
- Runs 9 and 10, `/proc`: wineserver at 79 and 76 % of a processor (it has one thread); the
  whole process at 1.8 processors of 24; the thread that maps asleep in `read` on the
  server's reply for 31 % of the time; the submitting thread on one futex for 33 % of its
  (taken to be ntdll's lock: 0.2 % once the mapping calls were gone).
- The report lines of run 9: 176 ms a frame "asking about title memory", 171 ms waiting for
  the guards, 164 ms of texture refreshes, the submitting thread at work 253 ms a frame.

What was changed (`crates/victory-hle/src/modules/wine_map.rs`, `VICTORY_WINE_MAP=0` turns
it off): under Wine the server is not asked. Where the host would have a placeholder the
reservation is a plain one. A page is shown by mapping the section's file there with the C
library's `mmap` (`MAP_SHARED | MAP_FIXED`; the descriptor is the server's own, from ntdll's
`wine_server_handle_to_fd`: the same pages as any view of the section) and then committing
the range, which under Wine sets its books and the protection and nothing else. It is taken
away by `MEM_DECOMMIT`, which under Wine maps fresh inaccessible memory over the range.
Protection changes, queries and faults go through Wine as before. A query calls such a page
private, not mapped, so the GPU side is told where these reservations are
(`victory_gpu::title_view_space`) and takes what is committed there for a view. The title's
own `VirtualFree` of placeholders (it splits them: kind 0x8002) is done on pieces kept of
the reservation.

`mmap` is called in the C library, not as a system call of our own: Wine turns on syscall
user dispatch for every thread and lets system calls through only from the C library's
mapping; one made from our code would be taken for a Windows system call. The library is
found in this process's `/proc/<pid>/maps` (the process is found by a marker file it maps:
Wine's server opens the files, so `/proc/self` is the server) and `mmap` in its dynamic
symbols. A page of a section of our own is shown this way at the start and read, written,
protected and taken away; if any of that fails, pages are shown as views, as before.

After (run 11, same log): 1,223 maps a second of about 3 us and 1,305 unmaps of 7 us, 13 ms
of every second; wineserver at 0.2 % of a processor; "asking about title memory" 3.9 ms a
frame, texture refreshes 10.8 ms, the submitting thread at work 42 ms a frame (253 before).
The frame: 287 ms, and now the same from line to line (243 to 314; 292 to 845 before).

The xmem tests pass under Wine both ways (20 of 20). With views, and all tests of the crate
running at once, `a_title_thread_goes_on_using_the_rest_of_a_view` failed once.

## Cause 2: six pipeline workers ended, and their pipelines were waited for in every frame

With cause 1 gone the frame stood at 287 ms with the card 18 % busy and the submitting
thread at work 42 ms. The timeline (`VICTORY_TIMELINE`, run 13): of four frames two took 35
and 75 ms and two 490 and 483 ms, and in those 475 ms were one wait of 500.05 ms on a
`victory-list` thread that nobody ended: `pipeline::make::wait`, the half second a frame
that a first use waits for its host pipeline. The frame line said "6 being made" from the
scene's first second to the end of every run, and 2.0 draws a frame were dropped as "being
made".

Six of the 18 pipeline workers were gone (a dump of all threads, run 14: numbers 2, 5, 6,
8, 12 and 14 missing). With Wine's `err` channel on (run 15) six lines say why:
`virtual_setup_exception stack overflow ... (0x28020000-0x28021000-0x28220000)`, all within
6 ms, 32 s after the start, while a batch of compute pipelines is made. Wine ends a thread
whose stack has run out and says so only on that channel, which `WINEDEBUG=-all` hides;
Windows would end the process. The pipeline stays "being made" for good, every frame that
uses it waits its whole budget, and the worker is lost.

Where the stack goes: in vkd3d-proton (`d3d12core.dll`), which turns the shader into the
host's form on the calling thread. A dump taken as it happens (run 18) shows such a thread
with two code addresses of `d3d12core.dll` in the 32 KiB above its stack pointer and
nothing else. The same six programs end their worker with a stack of 64 MiB (run 16) and of
1 GiB (run 18) as with 2 MiB: it is not a matter of a larger stack. They are compute
programs `17829ac71aab4e8d`, `a1b20bc47e3c9597`, `afeea48bbb9913ad`, `e8c962358d862e3f`,
`559d9f0184e15fe3` and `32ed072b9f845048`.

What was changed (`crates/victory-gpu/src/pipeline/make.rs`, `VICTORY_PIPELINE_WATCH=0`
turns it off): under Wine a thread watches the workers. When one ends, the pipeline it was
making is given up (its draws and dispatches are dropped at once, as those of a pipeline
the host refuses), a warning names its shaders, and another worker is started in its place.
On Windows nothing is watched and nothing changes. `VICTORY_PIPELINE_STACK_MB` sets the
workers' stacks, for looking into this; unset, they are as before.

After (run 19): 47.9 ms a frame, the card 90 % busy, the process at 10 processors.

## The README's switches under Wine

The picture was looked at in each of these: `VICTORY_CAPTURE` saves frames as they are
presented (with `VICTORY_CAPTURE_WORLD`, every eighth of the world's), and their mean
brightness was taken (0 to 255).

- **`VICTORY_PORT_CONSTANTS=1` (renderer patch 1): a black picture.** Every frame of the world
  is black (mean 0.2; 21 to 27 without it), with pages shown by `mmap` and with
  `VICTORY_WINE_MAP=0` alike. The log says the stretch of title memory was "opened as a host
  heap through a view of ours" with no complaint. Not looked into further: whether
  vkd3d-proton's heap from an address shares the memory or copies it is the first thing to
  ask. Runs 22 to 24 had it on and measured 33.3 ms (the title's own limit) and 25.8 ms
  (`VICTORY_FRAME_INTERVAL_MS=16.667`) of black frames: of no use but to say the submitting
  thread needs about 24 ms a frame when the card has little to do. Leave it off under Wine.
- `VICTORY_PORT_DESCRIPTORS=1`, `VICTORY_PORT_CBV=1`, `VICTORY_PORT_GUARD=1`,
  `VICTORY_PORT_JOURNAL=1`: the picture is there, and the frame is the same (48.2 ms in run
  26 against 47.9 in run 19): the card is the wall here, at 91 %.
- `VICTORY_OUTPUT=1920x1080`: 39.2 ms a frame (25.5 fps), the card 78 % busy, the picture
  there (run 27). The largest gain left that was measured.
- `--console series-s`: not run (`play.sh` passes no arguments to `victory run`).

## What the calls cost under Proton's Wine

`tests/fixtures/wine-bench`, microseconds a call, one thread and eight at once (each on
memory of its own):

| Call | 1 thread | 8 threads |
|---|---|---|
| `GetWriteWatch` with reset, 64 KiB, nothing written | 0.37 | 4.4 |
| `GetWriteWatch` with reset, 64 MiB, nothing written | 5.6 | 21 |
| `GetWriteWatch` with reset, 64 MiB, 256 pages written | 9.5 | 86 |
| first write to a write-watched page after a reset | 0.18 | 0.33 |
| `GetWriteWatch` of a view (refused) | 0.20 | 0.90 |
| `VirtualQuery` of a view | 2.8 | 12 |
| `VirtualProtect` of 64 KiB, read-only and back (the two) | 3.0 | 33 |
| a write to a read-only block, opened by a vectored handler | 2.4 | 44 |
| `MapViewOfFile3` into a placeholder and `UnmapViewOfFile2` (the two), few views | 7.5 | 77 |

Asking about 1 GiB with all of it written takes 2.1 ms, with nothing written 0.09 ms.
Proton's write watch is the kernel's (userfaultfd with `UFFD_FEATURE_WP_ASYNC`, and
`PAGEMAP_SCAN`): a call is cheap. Every call takes the one lock and masks signals around
it, so eight threads go one at a time (9 to 18 times the cost of one). That is true of the
title's frame too, and was not what made it slow: it became so when one thread held the
lock for a third of a millisecond at a time, 2,000 times a second.

## Measured and not the cause

- The implementation of `GetWriteWatch`: Arch's Wine (run 4, write watch by faults) and
  Proton's (run 7, the kernel's) gave the same frames, and under Proton's a call costs
  microseconds (the table above). The 1 to 25 ms "askings" of run 7 fit waits for the lock
  behind the mapping calls: in run 11 the askings of a frame take 3.9 ms in all. Arch's
  Wine was not run again.
- A larger stack for the pipeline workers (above).
- `VICTORY_CARD_AWAKE`, MangoHud, the swap: not looked at; none of them showed in what was
  measured (no swap in use by the process in any run: `VmSwap 0.0 GB`).

## What is not known, and what is next

1. What the six compute programs do, and what the picture loses without them. They never
   had a host pipeline under Wine (the frame line counted their draws as dropped before
   too). Why vkd3d-proton's conversion does not end for them is not known: its `dxil-spirv`
   is the suspect and the DLL has no symbols here. To do: convert the six shaders with
   `dxil-spirv` alone, on Linux, and report it there.
2. Wine's server could keep views in a tree; that is theirs to change. With it, the
   Windows way would do under Wine as well.
3. The frame's "0.0 on the GPU": not looked at. "The card about 90 % busy" is our own
   count of the time something of ours is queued on it.
4. `VICTORY_PROFILE` and `VICTORY_STACKS` under Wine: unwinding another thread's stack
   faults in Wine's `RtlVirtualUnwind` (run 12 ended the title as the profile began). Under
   Wine they now give the thread's instruction and the words of its stack that point into
   images, nearest first: enough to see where a thread is, not a true chain of callers.
5. Why `VICTORY_PORT_CONSTANTS=1` shows nothing under Wine (below).
6. The picture against Windows: the frames looked at are whole pictures (the title cards, the
   street at night with its lamps, cars and people, subtitles). People away from lamps are
   near black; whether that is the README's known darkness or more than it was not compared.

## Tools

In `~/Games/victory-logs/tools/`, outside the repository (its policy takes no Python files):

- `measure.py <name> [--seconds 200] [--from 100] [KEY=VALUE ...]`: one run, its log, and what
  `/proc` says of the process's threads over the window (processor time of each, and what
  they sleep in, 50 times a second), and of wineserver.
- `frame-stats.py <log> [from]`: medians of the report lines.
- `profile-top.py <file>`: a `VICTORY_PROFILE` file by thread.
- `capture-run.sh <name> <seconds> [KEY=VALUE ...]` and `brightness.py <folder>`: a short run
  with the presented frames saved, and how bright they are.

In the repository:

- `tests/fixtures/wine-bench`: the table above; build with
  `cargo xwin build --release --target x86_64-pc-windows-msvc` in its folder, run with Wine.
- `WINEDEBUG=err+all,fixme-all` as a `KEY=VALUE` of `play.sh`: what Wine itself complains
  of. Worth a look in any run that is slow for no reason seen.
