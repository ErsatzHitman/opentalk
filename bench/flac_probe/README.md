# flac_probe

Reproducible version of the throwaway program behind the **FLAC upload experiment** in `docs/BENCHMARK.md`
(PLAN 9.5). Standalone crate (own empty `[workspace]`, own gitignored `target/`); it never joins the main build.

```
cd bench/flac_probe
cargo run --release                 # everything (about 40 s)
cargo run --release -- --skip-upload  # size + encode time + round trip only
cargo run --release -- --skip-encode --verbose   # paced upload only, per-run total / last-write / max-late
```

Other flags: `--fixtures DIR`, `--spin` (spin the sub-600 us gaps instead of bursting), `--default-buffers`
(keep the OS default loopback socket buffers, see "Why 4 MiB socket buffers").

## What it does

1. **Size and encode time.** Reads `tests/fixtures/tts_clean.wav` and `tts_fillers.wav`, builds three seeded-noise variants
   (xorshift64*, uniform, +-30 LSB on both clips and +-100 LSB on the fillers clip, clamped to i16), encodes each
   with `flacenc` 0.5.1 (`default-features = false`, default config, fixed 4096-sample blocks, fat LTO), one warm-up
   plus 21 timed encodes, reports median and p95. Every FLAC is decoded with the pure-Rust `claxon` 0.4.3 and compared
   sample for sample with the source: all five are **lossless**. (ffmpeg verification is therefore no longer needed.)
   "WAV bytes" is the 44-byte-header WAV the app uploads (the fixtures carry a 46-byte header).
2. **Paced loopback upload.** A sender writes the body to 127.0.0.1 in 1460-byte writes; write `i` is due at
   `start + i * 1460 * 8 / rate` (deadline pacing, a late write goes out at once). Waits of 600 us or more use a
   **high-resolution waitable timer** (`CreateWaitableTimerExW` with `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`);
   shorter gaps (234 us per packet at 50 Mbps) are sent as a burst, so the average rate stays right. The receiver replies one byte when all bytes
   have arrived; the clock stops when the sender reads that reply. Median of 5 per cell, WAV and FLAC bodies of the 8.47 s
   fillers clip, clean and noise 30, at 2/5/10/20/50 Mbps. TLS and TCP slow start are out of scope.

## Machine

Intel Core i7-14650HX (24 logical processors), 23.7 GB, Windows 11 Home 10.0.26200, laptop on mains, desktop
locked (LogonUI) during the runs. rustc stable 1.99 (`x86_64-pc-windows-gnu`), `opt-level = 3`, fat LTO, codegen-units 1.

## Results

### Sizes (item 11)

| Clip (16 kHz mono) | WAV bytes | FLAC bytes | FLAC / WAV | BENCHMARK.md FLAC bytes | Delta | Round trip |
|---|---|---|---|---|---|---|
| tts_clean.wav, 6.03 s | 193,004 | 83,207 | 0.431 | 83,207 | 0.00% | lossless |
| tts_fillers.wav, 8.47 s | 271,244 | 128,297 | 0.473 | 128,297 | 0.00% | lossless |
| tts_clean.wav + noise 30 | 193,004 | 109,255 | 0.566 | 109,206 | +0.04% | lossless |
| tts_fillers.wav + noise 30 | 271,244 | 158,124 | 0.583 | 158,109 | +0.01% | lossless |
| tts_fillers.wav + noise 100 | 271,244 | 171,000 | 0.630 | 171,025 | -0.01% | lossless |

The clean clips are byte-exact with the BENCHMARK table; the noisy variants differ only because the old noise
generator is not recoverable (all within 0.05%, the acceptance bar was 5%).

### Encode time (ms, median / p95 of 21)

Encode time is sensitive to the laptop's CPU state. Four consecutive runs of this probe on the same binary:

| Clip | run 1 | run 2 | run 3 | run 4 | BENCHMARK.md |
|---|---|---|---|---|---|
| tts_clean.wav (6.03 s) | 9.5 / 11.2 | 5.1 / 6.1 | 3.8 / 3.9 | 3.9 / 4.2 | 9.6 / 12.1 |
| tts_fillers.wav (8.47 s) | 14.5 / 16.2 | 8.0 / 8.9 | 6.2 / 8.0 | 5.8 / 6.8 | 14.9 / 32.7 |
| tts_clean.wav + noise 30 | 11.3 / 16.5 | 6.1 / 7.7 | 4.2 / 4.4 | 4.4 / 5.8 | 11.2 / 24.3 |
| tts_fillers.wav + noise 30 | 15.9 / 35.1 | 8.7 / 9.4 | 6.5 / 7.5 | 6.2 / 6.8 | 16.5 / 29.1 |
| tts_fillers.wav + noise 100 | 23.0 / 27.5 | 9.4 / 11.7 | 6.1 / 8.3 | 6.1 / 7.8 | 15.7 / 36.4 |

Run 1 (cold, right after a build, same as BENCHMARK's conditions) reproduces the BENCHMARK figures; once the CPU has
settled the same code runs in 40-60% of that time (0.6-0.85 ms per audio second against the 1.76 / 1.94 ms that
BENCHMARK uses). So BENCHMARK's encode cost is the pessimistic end. It does not move the decision: at 50 Mbps an
8.5 s clean dictation saves 23 ms of upload and spends 6-8 ms encoding, still under the 30 ms bar; the noisy
cases save 18 ms or less.

### Paced upload (item 12), 4 MiB socket buffers, median of 5 (ms)

| Body | Uplink | Bytes | Formula `bytes*8/rate` | Measured | Deviation |
|---|---|---|---|---|---|
| fillers WAV | 2 Mbps | 271,244 | 1085 | 1081 | -0.4% |
| fillers FLAC | 2 Mbps | 128,297 | 513 | 509 | -0.9% |
| fillers + noise 30 WAV | 2 Mbps | 271,244 | 1085 | 1081 | -0.4% |
| fillers + noise 30 FLAC | 2 Mbps | 158,124 | 632 | 631 | -0.2% |
| fillers WAV | 5 Mbps | 271,244 | 434 | 432 | -0.4% |
| fillers FLAC | 5 Mbps | 128,297 | 205 | 204 | -0.8% |
| fillers + noise 30 WAV | 5 Mbps | 271,244 | 434 | 433 | -0.3% |
| fillers + noise 30 FLAC | 5 Mbps | 158,124 | 253 | 253 | -0.1% |
| fillers WAV | 10 Mbps | 271,244 | 217 | 216 | -0.2% |
| fillers FLAC | 10 Mbps | 128,297 | 103 | 102 | -0.7% |
| fillers + noise 30 WAV | 10 Mbps | 271,244 | 217 | 216 | -0.3% |
| fillers + noise 30 FLAC | 10 Mbps | 158,124 | 126 | 127 | +0.0% |
| fillers WAV | **20 Mbps** | 271,244 | 108 | 108 | -0.5% |
| fillers FLAC | **20 Mbps** | 128,297 | 51 | 51 | -0.2% |
| fillers + noise 30 WAV | **20 Mbps** | 271,244 | 108 | 108 | -0.3% |
| fillers + noise 30 FLAC | **20 Mbps** | 158,124 | 63 | 63 | -0.5% |
| fillers WAV | **50 Mbps** | 271,244 | 43 | 43 | +0.0% |
| fillers FLAC | **50 Mbps** | 128,297 | 21 | 20 | -2.0% |
| fillers + noise 30 WAV | **50 Mbps** | 271,244 | 43 | 43 | -0.7% |
| fillers + noise 30 FLAC | **50 Mbps** | 158,124 | 25 | 25 | +0.8% |

The 20 and 50 Mbps rows are now **measured** and agree with the formula to within 2% (the acceptance bar was 10%; the
-2.0% cell is a 0.4 ms difference on a 20.5 ms transfer), as do 2/5/10 Mbps (within 1%). The BENCHMARK net-saving
table, which used the formula for those two columns, stands unchanged. The worst per-write lateness the sender saw
was 0-2 ms (a late write goes out at once, so a late timer wake does not reduce the average rate).

### Why 4 MiB socket buffers

With the OS default loopback buffers the same test shows sporadic **+100 to +350 ms stalls** in about one run in five
(for example `fillers + noise 30 FLAC` at 20 Mbps: 63, 236, 225, 219, 272 ms), clustered in time. `--verbose`
shows them are not pacing: either one `write` blocks for 100-200 ms (send window full: `max-late` 100-200) or the
reply arrives 100-250 ms after the last write was issued (tail delivery waiting on a TCP timer). The sender's own
lateness in the other runs is 1 ms. Setting `SO_RCVBUF`/`SO_SNDBUF` to 4 MiB on both ends removes every stall (100 of
100 runs clean), so it is the default; `--default-buffers` reproduces the stalls. This is a loopback TCP artefact of
the harness, not something the real upload (a remote socket through TLS) is expected to hit, and it is why the
old run needed the formula rather than a measurement.

## Conclusion

The probe reproduces BENCHMARK.md's FLAC table: clean-clip sizes are byte-exact, every variant is within 0.05% and
decodes losslessly with a pure-Rust decoder, and encode time matches on a cold CPU (and is roughly half on a settled one).
The paced upload, now using a high-resolution waitable timer with deadline pacing and large loopback buffers, measures
the 20 and 50 Mbps rows within 2% of `bytes*8/rate`, so the formula-derived cells in the BENCHMARK net-saving table
were right and the decision (keep WAV; FLAC clears 30 ms net only on slow uplinks) is unchanged.
