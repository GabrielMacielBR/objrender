# objrender

An open-source, specialized **offline 3D object-based spatial audio renderer** written in Rust.

> **Status: Phase 2 Implemented (Timeline, Motion & DAMF/CAF Importer)**
> Supports moving 3D objects (`Linear` / `Spherical` SLERP / `Step`), per-sample linear gain ramping, Hann discontinuity crossfades, and decoded `truehdd`/`harletty` DAMF master sets (`.atmos` + `.atmos.metadata` + `.atmos.audio` CAF) rendered to arbitrary layouts as WAV/RF64.
>
> Quick CLI examples:
> ```sh
> # Render a decoded truehdd/harletty DAMF bundle to 7.1.4 WAV
> cargo run -- --damf movie.atmos --layout 7.1.4 --output rendered_7_1_4.wav
>
> # Render a moving test sine wave orbiting from Left (+90°) to Right/Top (-90°, +45°)
> cargo run -- --layout 7.1.4 --az 90 --el 0 --pan-to -90,45 --duration 2.0 --output orbit.wav
> ```

---

## Motivation

Open-source decoders for object-based surround bitstreams (such as `truehdd` and `harletty`) make it possible to extract discrete audio objects and OAMD/DAMF spatial metadata (CAF audio + ETSI TS 103 420 metadata), yet few open-source tools exist to perform clean offline rendering of those objects onto **arbitrary speaker setups** (from Stereo and 5.1.4 up to 9.1.6 or custom speaker geometries) as standard multichannel **WAV / RF64** files.

`objrender` fills that gap with a lean, offline-first renderer built in safe Rust.

---

## High-Level Architecture

The rendering core is strictly decoupled from any input format. Format-specific importers translate decoded streams into a generic 3D object-based `Scene` representation:

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
                                  Spatial Renderer (e.g., 3D VBAP)
                                                 │
                                                 ▼
                               Mixer (summation, headroom, clipping)
                                                 │
                                                 ▼
                                              PCM Bus
                                                 │
                                                 ▼
                                      Wave Writer (WAV / RF64)
```

---

## Core Design Principles

- **Format-Agnostic Core**: The rendering engine knows nothing about external container or metadata standards (`OAMD`/`DAMF`, `ADM`, `IAMF`, `MPEG-H`)—it operates purely on 3D audio objects, keyframes, and speaker geometry.
- **Arbitrary Speaker Layouts**: No hardcoded channel counts or fixed speaker positions. Every speaker is modeled by its 3D geometric coordinates.
- **Offline Specialization**: Built specifically for accurate, deterministic offline file rendering—not as a DAW, real-time media player, or generic audio framework.
- **Lean, Safe Rust**: Prioritizes clarity, safety, performance, and maintainability without ECS, plugin systems, or unnecessary `unsafe`.

---

## Roadmap

- **Phase 1 (MVP)**: `Scene` model, arbitrary speaker layouts, single static object rendering, WAV export.
- **Phase 2**: Moving objects, keyframes, timeline interpolation & temporal smoothing.
- **Phase 3**: Full 3D VBAP, energy normalization, object spread/width.
- **Phase 4**: SIMD acceleration.
- **Phase 5**: Multi-threaded offline parallelization.
- **Phase 6**: RF64 (>4 GB) export, full format importer integration, advanced mixing/loudness controls.

---

## Development Guidelines

Architectural rules, subsystem requirements, and design constraints are documented in [CLAUDE.md](./CLAUDE.md).

---

## License

Licensed under the **GNU General Public License v3.0 or later** (`GPL-3.0-or-later`, see [LICENSE](./LICENSE) or <https://www.gnu.org/licenses/gpl-3.0.html>).

---

## Attribution & Independence Notice

`objrender` is an independent open-source project implementing offline spatial audio rendering (VBAP, RF64, OAMD / ETSI TS 103 420, and ITU-R BS.2076 interoperability). Its pure-Rust 3D VBAP triangulation and panning core adapts modules from [Omniphony](https://github.com/mgth/Omniphony) (`GPL-3.0-or-later`), building upon the Spatial Audio Framework (`SAF`) / Archontis Politis / Leo McCormack / Ville Pulkki VBAP lineage. `objrender` is not affiliated with, endorsed by, or sponsored by any commercial vendor or standards organization.
