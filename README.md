# objrender

An open-source, offline 3D object-based spatial audio renderer written in safe Rust (`#![forbid(unsafe_code)]`).

> **Status: Phases 1–6 Complete**
> Offline rendering from decoded `truehdd` / `harletty` DAMF master sets (`.atmos` + `.atmos.metadata` + `.atmos.audio` CAF) or synthetic scenes onto 18 built-in speaker layouts (mono through `9.1.6` & `auro-10.1`) or arbitrary TOML geometries, accelerated by runtime-dispatched SIMD (`pulp`) and deterministic `rayon` multi-threading, exporting `i16`/`i24`/`f32`/`f64` WAV/RF64 (`>4 GiB`) or raw/WAV stdout pipes (`--output -`) with optional ITU-R BS.1770-4 loudness normalization.

---

## Features

- **Pure-Rust 1D / 2D / 3D VBAP Spatializer**
  - Exact 3D convex-hull triangulation with L2 energy conservation and LFE isolation.
  - Spatial `width` spread (MDAP / directional diffusion) and omnidirectional `divergence` mixing.
  - Smooth near-origin omnidirectional blending (`--near-origin-radius`, default `0.15`) to eliminate directional snapping near the listener center.
  - Configurable out-of-hull elevation handling: `virtual-poles` (default), `blend[:pow]` (default power `12.0`), or `fade`.
- **Sample-Accurate Timeline & Motion**
  - Sorted keyframe tracks with `Linear`, `Spherical` (SLERP on unit directions + linear radius), and `Step` interpolation.
  - Per-sample linear gain ramping and raised-cosine (Hann) anti-click crossfades across discontinuous keyframes (including across chunk boundaries).
- **Arbitrary Speaker Layouts**
  - 18 built-in presets: `mono` (`1.0`), `stereo` (`2.0`), `2.1`, `3.1.2`, `4.0` (`quad`), `4.1`, `5.0`, `5.1`, `5.1.2`, `5.1.4`, `7.0`, `7.1`, `7.1.2`, `7.1.4`, `7.1.6`, `9.1.4`, `9.1.6`, and `auro-10.1`.
  - Selectable surround ordering: `--channel-order wav` (default `WAVE_FORMAT_EXTENSIBLE`) or `--channel-order smpte` (`Lss`/`Rss` before `Lrs`/`Rrs`).
  - Custom arbitrary speaker setups loaded from TOML (`--layout-file`) in spherical (`azimuth_deg`, `elevation_deg`, `radius`) or Cartesian (`x`, `y`, `z`) coordinates.
- **Mastering, Mixing & Streaming I/O**
  - Clipping & loudness policies: pass-through, static headroom attenuation (`--headroom-db`), exact two-pass global peak normalization (`--normalize-peak`), or ITU-R BS.1770-4 integrated loudness normalization with 4x oversampled true-peak limiting (`--normalize-lufs`, `--true-peak-ceiling`).
  - Missing-LFE routing policy: `drop` (default) or `folddown[:db]` (default `-3.0 dB`).
  - Export formats: `--format i16`, `i24`, `f32`, `f64` with deterministic SplitMix64 TPDF dithering (`--dither auto|none|tpdf`, enabled automatically for `i16`).
  - File WAV / automatic O(1) RF64 (`ds64` for `>4 GiB`) plus non-seeking streaming WAV or `--raw-pcm` output to stdout (`--output -`) for zero-disk-overhead piping into `ffmpeg` (`FLAC`, `Opus`, `AAC`, `IAMF`, etc.).
  - Full `truehdd` / `harletty` DAMF + 16/24/32-bit PCM & 32/64-bit float CAF support (including `EAC3-JOC` core channel skipping and Dolby room-cube spherical projection).
- **SIMD & Deterministic Parallelism**
  - Runtime-dispatched SIMD kernels via [`pulp`](https://crates.io/crates/pulp) with `x86-v4` enabled (AVX-512, AVX2+FMA, SSE, NEON, or scalar fallback) for ramp accumulation, peak/RMS metering, and tiled PCM interleaving.
  - Lock-split, recycled multi-slot CAF decoding cache paired with `rayon` chunked parallel rendering (`--threads`).
  - Guaranteed **bit-for-bit identical** output across thread counts (`--threads 1` vs `--threads 0`), even with TPDF dither or two-pass LUFS normalization.

---

## Building

Requires a Rust toolchain supporting **Edition 2024**.

```sh
git clone https://github.com/GabrielMacielBR/objrender.git
cd objrender
cargo build --release
```

The compiled binary will be located at `./target/release/objrender`.

---

## Quick Usage

### 1. Render a Decoded DAMF Master Set

If you have a Dolby Atmos stream (TrueHD or E-AC-3 JOC), first decode it to a DAMF master set using `truehdd` or `harletty` + `ffmpeg`, then render onto your target speaker layout:

```sh
# Decode E-AC-3 JOC audio to DAMF bundle with harletty (example)
ffmpeg -i movie.mp4 -vn -c:a copy audio.eac3
harletty decode --output-path damf_out/ audio.eac3

# Default render to 7.1.4 WAV (auto threads, 32-bit float)
./target/release/objrender --damf damf_out/master.atmos --layout 7.1.4 --output render_7_1_4.wav

# Broadcast / streaming mastering: -18 LUFS integrated, -1.0 dBTP true-peak ceiling, 24-bit PCM WAV
./target/release/objrender \
  --damf damf_out/master.atmos \
  --layout 5.1.2 \
  --normalize-lufs -18.0 \
  --true-peak-ceiling -1.0 \
  --format i24 \
  --output master_5_1_2_24bit.wav

# Zero-temp-file stdout pipe directly into ffmpeg (e.g., 24-bit multichannel FLAC)
./target/release/objrender \
  --damf damf_out/master.atmos \
  --layout 3.1.2 \
  --normalize-peak -1.0 \
  --format i24 \
  --output - | ffmpeg -i - -c:a flac -compression_level 8 render_3_1_2.flac

# 64-bit float output for 9.1.6 using SMPTE surround order (7.1.4 example below) and folded LFE
./target/release/objrender \
  --damf damf_out/master.atmos \
  --layout 4.0 \
  --format f32 \
  --threads 4 \
  --lfe-policy folddown:-3.0 \
  --output quad_folddown.wav
```

### 2. Render Synthetic Test Signals

```sh
# Orbiting 1 kHz sine wave from Left (+90 deg) to Right/Top (-90 deg, +45 deg) over 2 seconds
cargo run --release -- \
  --layout 7.1.4 \
  --az 90 --el 0 \
  --pan-to -90,45 \
  --interp spherical \
  --duration 2.0 \
  --output orbit.wav

# Static object placed using normalized [0,1]x[0,1]x[-1,1] DAMF room-cube coordinates with width/divergence
cargo run --release -- \
  --layout auro-10.1 \
  --damf-pos 0.25,0.25,0.5 \
  --width 0.3 --divergence 0.15 \
  --output auro_test.wav
```

### 3. Custom TOML Speaker Layout

Coordinate convention: `+X` = Right, `+Y` = Front, `+Z` = Up (`azimuth = 0°` Front, `+90°` Left, `-90°` Right; `elevation = +90°` Zenith). Each speaker specifies either `(azimuth_deg, elevation_deg[, radius])` or `(x, y, z)`.

```toml
name = "custom-quad-plus-top"

[[speakers]]
label = "FL"
azimuth_deg = 35.0
elevation_deg = 0.0

[[speakers]]
label = "FR"
azimuth_deg = -35.0
elevation_deg = 0.0

[[speakers]]
label = "RL"
azimuth_deg = 125.0
elevation_deg = 0.0

[[speakers]]
label = "RR"
azimuth_deg = -125.0
elevation_deg = 0.0

[[speakers]]
label = "LFE"
role = "lfe"
x = 0.0
y = 0.0
z = 0.0

[[speakers]]
label = "TOP"
azimuth_deg = 0.0
elevation_deg = 85.0
radius = 1.0
```

```sh
./target/release/objrender --damf damf_out/master.atmos --layout-file custom.toml --output custom.wav
```

---

## CLI Options Reference

| Flag | Description | Default |
| :--- | :--- | :--- |
| `--damf <path.atmos>` | Path to decoded `truehdd` / `harletty` DAMF manifest | Synthetic test sine |
| `--layout <name>` | Speaker preset (18 presets, `mono` through `9.1.6` & `auro-10.1`) | `7.1.4` |
| `--layout-file <path>` | Custom TOML speaker layout file (overrides `--layout`) | None |
| `--channel-order <order>` | Multichannel order: `wav` or `smpte` (`Lss`/`Rss` before `Lrs`/`Rrs`) | `wav` |
| `--az <deg>` | Synthetic object initial azimuth in degrees | `0.0` |
| `--el <deg>` | Synthetic object initial elevation in degrees | `0.0` |
| `--radius <r>` | Synthetic object distance radius | `1.0` |
| `--pan-to <az,el>` | Pan synthetic sine wave from `(--az,--el)` to `(az,el)` over `--duration` | None |
| `--interp <mode>` | Trajectory interpolation for `--pan-to`: `linear`, `spherical`, `step` | `spherical` |
| `--damf-pos <x,y,z>` | Synthetic static DAMF room-cube coords (`x,y` in `[0,1]`, `z` in `[-1,1]`) | None |
| `--width <0..1>` | Spatial spread width for synthetic object | `0.0` |
| `--divergence <0..1>` | Omnidirectional divergence diffusion for synthetic object | `0.0` |
| `--near-origin-radius <r>` | Smooth omnidirectional blend radius near origin (`0` disables) | `0.15` |
| `--out-of-hull <mode>` | Out-of-hull behavior: `virtual-poles`, `blend[:pow]`, or `fade` | `virtual-poles` |
| `--gain <linear>` | Synthetic object linear gain | `1.0` |
| `--freq <hz>` | Synthetic sine wave frequency in Hz | `1000.0` |
| `--duration <sec>` | Synthetic signal duration in seconds | `1.0` |
| `--sample-rate <hz>` | Synthetic signal sample rate in Hz | `48000` |
| `--headroom-db <db>` | Static output attenuation in dB (exclusive with `--normalize-*`) | None |
| `--normalize-peak <dbfs>` | Two-pass global peak normalization target in dBFS (`<= 0.0`) | None |
| `--normalize-lufs <lufs>` | Two-pass ITU-R BS.1770-4 integrated loudness target (`<= 0.0`) | None |
| `--true-peak-ceiling <dbtp>` | True-peak ceiling in dBTP used with `--normalize-lufs` (`<= 0.0`) | `-1.0` |
| `--lfe-policy <policy>` | Missing-LFE behavior: `drop` or `folddown[:db]` (`-3.0` default dB) | `drop` |
| `--crossfade-frames <n>` | Anti-click Hann crossfade length in frames on discontinuities (`0` disables) | `128` |
| `--threads <n>` | Worker threads (`0` = auto via Rayon, `1` = single-threaded) | `0` |
| `--format <fmt>` | Output format: `i16`, `i24`, `f32`, or `f64` | `f32` |
| `--dither <mode>` | Integer PCM dither: `auto` (TPDF on `i16`), `none`, or `tpdf` | `auto` |
| `--raw-pcm` | Emit headerless interleaved little-endian PCM instead of WAV container | Off |
| `--output <path\|->` | Output file path, or `-` to stream directly to `stdout` | `render.wav` |
| `-h`, `--help` | Print CLI help | — |

---

## Built-in Speaker Presets & Channel Ordering

| Preset | Aliases | Channels | Channel Labels (`wav` order) | `WAVE_FORMAT_EXTENSIBLE` Mask |
| :--- | :--- | :---: | :--- | :--- |
| `mono` | `1.0` | 1 | `C` | `0x0004` |
| `stereo` | `2.0` | 2 | `L, R` | `0x0003` |
| `2.1` | — | 3 | `L, R, LFE` | `0x000B` |
| `3.1.2` | — | 6 | `L, R, C, LFE, Ltf, Rtf` | `0x500F` |
| `4.0` | `quad` | 4 | `L, R, Ls, Rs` | `0x0033` |
| `4.1` | — | 5 | `L, R, LFE, Ls, Rs` | `0x003B` |
| `5.0` | — | 5 | `L, R, C, Ls, Rs` | `0x0037` |
| `5.1` | — | 6 | `L, R, C, LFE, Ls, Rs` | `0x003F` |
| `5.1.2` | — | 8 | `L, R, C, LFE, Ls, Rs, Ltm, Rtm` (`±90°, +45°`) | `0x503F` |
| `5.1.4` | — | 10 | `L, R, C, LFE, Ls, Rs, Ltf, Rtf, Ltr, Rtr` | `0xD03F` |
| `7.0` | — | 7 | `L, R, C, Lrs, Rrs, Lss, Rss` | `0x0637` |
| `7.1` | — | 8 | `L, R, C, LFE, Lrs, Rrs, Lss, Rss` | `0x063F` |
| `7.1.2` | — | 10 | `L, R, C, LFE, Lrs, Rrs, Lss, Rss, Ltm, Rtm` (`±90°, +45°`) | `0x563F` |
| `7.1.4` | — | 12 | `L, R, C, LFE, Lrs, Rrs, Lss, Rss, Ltf, Rtf, Ltr, Rtr` | `0x2D63F` |
| `7.1.6` | — | 14 | `L, R, C, LFE, Lrs, Rrs, Lss, Rss, Ltf, Rtf, Ltm, Rtm, Ltr, Rtr` | `0` |
| `9.1.4` | — | 14 | `L, R, C, LFE, Lw, Rw, Lss, Rss, Lrs, Rrs, Ltf, Rtf, Ltr, Rtr` | `0` |
| `9.1.6` | — | 16 | `L, R, C, LFE, Lw, Rw, Lss, Rss, Lrs, Rrs, Ltf, Rtf, Ltm, Rtm, Ltr, Rtr` | `0` |
| `auro-10.1` | `auro10.1`, `10.1` | 11 | `L, R, C, LFE, Ls, Rs, HL, HR, HLs, HRs, VoG` (`+90°` zenith) | `0` |

With `--channel-order smpte`, `7.0`, `7.1`, `7.1.2`, `7.1.4`, and `7.1.6` place side surrounds (`Lss`, `Rss`) before rear surrounds (`Lrs`, `Rrs`), matching cinema/SMPTE DAW conventions (and set `dwChannelMask = 0` so Microsoft WAV parsers do not misinterpret reordered surround bits).

---

## Library Usage

`objrender` can also be used as a Rust crate:

```rust
use objrender::importers::SceneImporter;
use objrender::importers::damf::DamfImporter;
use objrender::io::{SampleFormat, WaveConfig, WaveWriter};
use objrender::layout::{ChannelOrder, SpeakerLayout};
use objrender::mixer::{ClippingPolicy, RenderConfig, render_scene};
use objrender::renderer::VbapRenderer;

fn render_damf_to_wav() -> Result<(), Box<dyn std::error::Error>> {
    let layout = SpeakerLayout::preset_with_order("7.1.4", ChannelOrder::Wav).unwrap();
    let mut scene = DamfImporter::new("master.atmos").import()?;
    let spatializer = VbapRenderer::build(&layout)?;

    let writer = WaveWriter::create(
        "output_7_1_4_i24.wav",
        WaveConfig::new(
            scene.sample_rate,
            layout.speakers.len() as u16,
            layout.channel_mask(),
            SampleFormat::Int24,
        ),
    )?;

    let report = render_scene(
        &mut scene,
        &layout,
        &spatializer,
        RenderConfig {
            clipping: ClippingPolicy::TwoPassLoudnessNormalize {
                target_lufs: -18.0,
                true_peak_ceiling_dbtp: -1.0,
            },
            threads: 0, // auto via Rayon
            ..RenderConfig::default()
        },
        Box::new(writer),
    )?;

    println!(
        "Rendered {} frames (gain={:+.2} dB, lufs={:.2}, tp={:.2} dBTP)",
        report.frames_rendered,
        report.applied_gain_db(),
        report.integrated_lufs.unwrap_or(f64::NEG_INFINITY),
        report.true_peak_dbtp.unwrap_or(f64::NEG_INFINITY),
    );
    Ok(())
}
```

---

## Architecture & Core Design Principles

The rendering engine is strictly decoupled from input container and metadata formats. Importers convert decoded streams into a format-agnostic `Scene` of `AudioObject` tracks and keyframe trajectories:

```text
  Decoded Streams (truehdd / harletty: DAMF/OAMD + CAF)   Future Formats (ADM / IAMF / MPEG-H)
                      │                                                    │
                      ▼                                                    ▼
              DAMF/OAMD Importer                                    Format Importer
                      └──────────────────────────┬─────────────────────────┘
                                                 ▼
                                           Scene Builder
                                                 │
                                                 ▼
                              Scene (generic 3D AudioObjects + PCM)
                                                 │
                                                 ▼
                                  Spatial Renderer (1D/2D/3D VBAP)
                                                 │
                                                 ▼
                    Parallel Chunked Mixer (ramping, crossfade, LFE, SIMD)
                                                 │
                                                 ▼
                                    Planar f64 PCM Bus + Metering
                                                 │
                                                 ▼
                        Wave / Streaming Pipe / Raw PCM Writer (i16..f64, RF64)
```

- **Format-Agnostic Core**: Core modules (`math`, `layout`, `scene`, `timeline`, `renderer`, `mixer`, `io`) never reference `importers`, `damf`, `caf`, or `serde_yaml`.
- **Safe & Lean**: `#![forbid(unsafe_code)]` crate-wide across library and binary, without ECS/plugin/real-time DAW bloat.
- **Further Reading**: Detailed design rules and subsystem specs are documented in [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) and [CLAUDE.md](./CLAUDE.md).

---

## Testing & Benchmarks

Run the full verification suite and Criterion microbenchmarks:

```sh
# Format, strict linting, and all unit/integration/property tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# Criterion SIMD & full-scene render benchmarks
cargo bench --bench dsp_kernels
```

### Measured Performance (AMD Ryzen 5 4600G — AVX2+FMA, 12 threads)

- **Real Atmos stream** (208.03s / 9,985,536 frames @ 48 kHz, 16 objects, 21-ch 600 MiB CAF → 479 MB `7.1.4` `f32` WAV):
  - **1-pass (`PassThrough`)**: `~2.17s` (`--threads 1`, `~96x` realtime) / `~2.25s` (`--threads 0`, disk read/write I/O bound)
  - **2-pass (`--normalize-peak -1.0`)**: `~4.19s` (`--threads 1`) → **`~2.70s` (`--threads 0`, `~77x` realtime, `1.55x` speedup including full disk write)**
  - **Determinism**: Bit-exact identical `SHA-256` output between `--threads 1` and `--threads 0` on both 1-pass and 2-pass renders.
- **Synthetic 2s scene (`render_scene_7_1_4_16obj`, `NullWriter`)**: `33.1 ms` (`1_thread`) → `9.0 ms` (`auto_threads`, **`~3.68x` speedup**).

---

## Roadmap

- [x] **Phase 1 (MVP)**: Format-agnostic `Scene`, arbitrary TOML & preset speaker layouts, 1D/2D/3D VBAP, WAV/RF64 (`>4 GiB`) export.
- [x] **Phase 2**: Keyframe timeline (`Linear`, `Spherical` SLERP, `Step`), sample-accurate gain ramping, Hann discontinuity crossfades, and streaming DAMF/OAMD + CAF importer (`truehdd` & `harletty` `EAC3-JOC`).
- [x] **Phase 3**: Smooth near-origin omnidirectional blending, omnidirectional `divergence`, out-of-hull modes (`virtual-poles`, `blend`, `fade`), LFE policies, headroom & two-pass global peak normalization.
- [x] **Phase 4**: Runtime-dispatched SIMD mixing, metering, and interleaved WAV export via `pulp` (`x86-v4` / AVX-512 / AVX2+FMA / SSE / NEON / scalar) + Criterion benchmarks.
- [x] **Phase 5**: Lock-split parallel CAF decoding, deterministic chunked `rayon` multi-threading (`--threads`), and bit-exact verification suite.
- [x] **Phase 6**: Extended presets (`2.1`, `3.1.2`, `4.0`, `4.1`, `5.0`, `7.0`, `5.1.2`, `7.1.2`, `9.1.4`, `auro-10.1`), `i16`/`i24` integer PCM + deterministic TPDF dither, non-seeking stdout/raw PCM pipe writers (`--output -`, `--raw-pcm`), and ITU-R BS.1770-4 loudness + 4x true-peak two-pass normalization (`--normalize-lufs`, `--true-peak-ceiling`).
- [ ] **Phase 7+**: Additional spatial formats (ADM BWF / `axml`, Ambisonics / IAMF export), binaural HRTF rendering (`.sofa`), and release packaging.

---

## AI-Assisted Development

Portions of this codebase, architecture documentation, tests, and benchmarks were drafted and iterated with **AI coding assistants** under human direction, design review, and validation. All core invariants and behaviors remain continuously verified via `#![forbid(unsafe_code)]`, strict `clippy` checks (`-D warnings`), automated unit/integration/property tests (including bit-exact determinism checks across thread counts), and real-world Atmos comparison renders against reference decoders/renderers.

---

## License

Licensed under the **GNU General Public License v3.0 or later** (`GPL-3.0-or-later`, see [LICENSE](./LICENSE) or <https://www.gnu.org/licenses/gpl-3.0.html>).

---

## Attribution & Independence Notice

`objrender` is an independent open-source project implementing offline spatial audio rendering (VBAP, RF64, OAMD / ETSI TS 103 420, ITU-R BS.1770-4, and ITU-R BS.2076 interoperability). Its pure-Rust 3D VBAP triangulation, panning, and room-cube projection modules adapt code from [Omniphony](https://github.com/mgth/Omniphony) (`GPL-3.0-or-later`), building upon the Spatial Audio Framework (`SAF`) / Archontis Politis / Leo McCormack / Ville Pulkki VBAP lineage. `objrender` is not affiliated with, endorsed by, or sponsored by any commercial vendor or standards organization.
