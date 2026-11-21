# objrender

An open-source, specialized **offline 3D object-based spatial audio renderer** written in Rust.

> **Status: Early Design / Pre-Alpha**
> The repository is currently in the initial architecture and design phase. Architecture documentation, module specs, and crate selections are being finalized before the first Rust code is committed. See [CLAUDE.md](./CLAUDE.md) for full engineering guidelines.

---

## Motivation

Open-source decoders for object-based surround bitstreams (such as `truehdd` / `LibTrueHD`) make it possible to extract discrete audio objects and spatial metadata, yet few open-source tools exist to perform clean offline rendering of those objects onto **arbitrary speaker setups** (from Stereo and 5.1.4 up to 9.1.6 or custom speaker geometries) as standard multichannel **WAV / RF64** files.

`objrender` fills that gap with a lean, offline-first renderer built in safe Rust.

---

## High-Level Architecture

The rendering core is strictly decoupled from any input format. Format-specific importers translate decoded streams into a generic 3D object-based `Scene` representation:

```text
  Decoded Atmos (e.g., LibTrueHD / truehdd)          Future Formats (e.g., MPEG-H)
                      │                                            │
                      ▼                                            ▼
               Atmos Importer                               Format Importer
                      └──────────────────┬─────────────────────────┘
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

- **Format-Agnostic Core**: The rendering engine knows nothing about Dolby, Atmos, TrueHD, or MPEG-H—it operates purely on 3D audio objects, keyframes, and speaker geometry.
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

## Trademark & Clean-Room Notice

`objrender` is an independent open-source project and is not affiliated with, endorsed by, or sponsored by Dolby Laboratories or any standards organization. Terms such as *Dolby*, *Dolby Atmos*, *TrueHD*, and *MPEG-H* are trademarks of their respective owners and are mentioned solely to describe interoperability with decoded audio formats.

Mathematical concepts (such as VBAP and geometric speaker panning) follow established academic literature and conceptual references cleanly implemented under `MIT OR Apache-2.0`.
