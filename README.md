# objrender

An open-source, specialized **offline 3D object-based spatial audio renderer** written in Rust.

> **Status: Early Design / Pre-Alpha**
> The repository is currently in the initial architecture and design phase. Architecture documentation, module specs, and crate selections are being finalized before the first Rust code is committed. See [CLAUDE.md](./CLAUDE.md) for full engineering guidelines.

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

Dual-licensed under either of:

- MIT License ([LICENSE-MIT](./LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

---

## Clean-Room & Independence Notice

`objrender` is an independent clean-room open-source project implementing standard spatial audio math (VBAP, RF64, OAMD / ETSI TS 103 420, and ITU-R BS.2076 interoperability) under `MIT OR Apache-2.0`. It is not affiliated with, endorsed by, or sponsored by any commercial vendor or standards organization.
