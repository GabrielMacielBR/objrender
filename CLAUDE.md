# CLAUDE.md

Engineering guidelines, architectural boundaries, and development roadmap for **objrender**.

---

## 1. Project Overview & Mission

**objrender** is an open-source, specialized **offline** 3D object-based spatial audio renderer written in Rust.

Its primary practical goal is to render decoded object-based cinema and home-theater audio content (such as Dolby Atmos streams decoded via `LibTrueHD` / `truehdd`, e.g., DAMF/OAMD + multichannel PCM) onto **arbitrary speaker layouts**, producing multichannel PCM buffers and **WAV / RF64** output files.

While Atmos interoperability is the motivating use case, the rendering core itself must remain **completely format-agnostic**, operating exclusively on a generic 3D object-based intermediate scene representation.

---

## 2. Project Philosophy

**objrender IS:**
- A focused, lean, offline spatial audio renderer.
- Designed for a small engineering team prioritizing clarity, correctness, and fast iteration to working audio output.

**objrender IS NOT:**
- A Digital Audio Workstation (DAW).
- A real-time media player or playback daemon.
- A general-purpose multimedia framework.
- A universal spatial audio engine trying to solve every audio problem.

---

## 3. Core Architectural Boundary (Strict Decoupling)

There must be a hard separation between **Format Importers** and the **Rendering Core**:

1. **Format Importer Layer** (e.g., `src/atmos/`):
   - Reads format-specific inputs (decoded bitstreams, metadata files, PCM tracks).
   - Interprets format-specific spatial metadata (e.g., OAMD / DAMF beds, object coordinates, size/divergence, trims).
   - Translates them into the generic intermediate `Scene` model via a Scene Builder.

2. **Rendering Core** (`scene`, `layout`, `renderer`, `timeline`, `mixer`, `io`, `math`):
   - Knows **only** about generic audio objects, scenes, speaker geometry, spatialization, temporal interpolation, mixing, and PCM/WAV export.
   - **MUST NEVER** depend on, import, or reference format-specific concepts such as `Atmos`, `Dolby`, `TrueHD`, or `MPEG-H`.

### Target Data Pipelines

Primary target workflow:

```text
TrueHD Atmos
  ↓
LibTrueHD / truehdd
  ↓
Atmos Importer
  ↓
Scene Builder
  ↓
Scene (generic 3D objects + audio)
  ↓
Spatial Renderer
  ↓
Mixer
  ↓
PCM Bus
  ↓
Wave Writer (WAV / RF64)
```

Future format extensibility (with **zero modifications** to the rendering core):

```text
MPEG-H (or other object-based format)
  ↓
Format Importer
  ↓
Scene
  ↓
Spatial Renderer → Mixer → PCM / Wave Writer
```

---

## 4. Clean-Room & Legal Hygiene

### Reference Policy (Omniphony)
- [Omniphony](https://github.com/mgth/Omniphony) may be consulted strictly as a **mathematical and conceptual reference** for:
  - VBAP (Vector Base Amplitude Panning) geometry and gain calculation
  - Geometric speaker modeling
  - Spatial rendering math
  - Arbitrary speaker layouts
  - Energy distribution and normalization
- **DO NOT** copy, port, or derive source code or software architecture from Omniphony (Omniphony is licensed under `GPL-3.0`, whereas `objrender` is clean-room dual-licensed under `MIT OR Apache-2.0`).
- **DO NOT** reproduce Omniphony's complexity, runtime ABI/plugin bridges, real-time audio server integrations, or universal-engine abstractions.

### Trademark Hygiene
- **NEVER** use third-party trademarks (such as `Dolby` or `Atmos`) in the project name, crate names, binary names, or public core API symbols.
- Reference external formats neutrally and strictly for interoperability documentation and format-specific importer modules (e.g., `src/atmos/`).

---

## 5. Intermediate Scene Model

The rendering core operates on a format-neutral intermediate representation:

- **`Scene`**:
  - Total duration (samples or time + sample count)
  - Sample rate (Hz)
  - Collection of `AudioObject` instances
- **`AudioObject`**:
  - Unique identifier (`id`)
  - Initial / static 3D position
  - Base gain
  - Spatial width / spread / size
  - Divergence parameters
  - Priority
  - Associated mono PCM audio stream (or source reference)
  - Keyframes
  - Automation curves / parameters
- **`Keyframe`**:
  - Timestamp (sample index or time offset)
  - 3D position
  - Gain
  - Spatial parameters (width, divergence, etc.)

*(Note: Channel beds from decoded streams can be represented either as fixed-position virtual objects pinned to standard coordinates or via layout-aware bed mapping in the importer—details to be finalized during the architecture phase.)*

---

## 6. Arbitrary Speaker Layouts

The renderer must support arbitrary 2D and 3D speaker configurations, including but not limited to:
- `Stereo` (2.0)
- `5.1`, `7.1`
- `5.1.4`, `7.1.4`, `7.1.6`, `9.1.6`
- Arbitrary custom speaker arrangements (asymmetric rooms, domes, irregular rigs)

**Strict rules for layouts:**
- **NEVER** assume a fixed channel count.
- **NEVER** hardcode fixed speaker positions in the core renderer.
- **NEVER** bake Atmos-specific layout assumptions into the rendering math.
- Represent every speaker explicitly by its **3D geometric position** (and channel index/role metadata as needed for WAV channel ordering or LFE handling).

---

## 7. DSP & Subsystem Design Requirements

During architecture design and implementation, each subsystem must address the following requirements:

### Spatialization Engine
- Compare candidate spatialization strategies across **subjective quality**, **computational cost**, **implementation simplicity**, and **suitability for Atmos-style discrete object content**:
  - **VBAP** (Vector Base Amplitude Panning)
  - **DBAP** (Distance-Based Amplitude Panning)
  - **MDAP** (Multiple Direction Amplitude Panning)
  - **Internal Ambisonics + Decode** (HOA intermediate bus + speaker decoder)
  - **Hybrid approaches**
- **MVP Recommendation**: Prefer **VBAP** if technically suitable—it is well-understood, computationally light, preserves point-source localization on arbitrary triangulated hulls, and naturally supports object width/spread extensions (e.g., MDAP/multi-point spreading).
- **Advanced Evolution**: Ensure the `Renderer` design allows upgrading spatialization strategies (e.g., spread models, hybrid bed/object handling, or Ambisonics) without restructuring the core architecture.

### Temporal Processing (`src/timeline/`)
Must account for:
- Continuous object movement across time
- Keyframe interpolation (linear, spherical/geodesic, or ramp-aligned interpolation)
- Parameter automation (gain, width, divergence)
- Temporal smoothing to prevent zipper noise / audible stepping
- Crossfades and ramp boundaries
- Constant-power / energy preservation during panning transitions

### Mixing (`src/mixer/`)
Must define explicit strategies for:
- Summing multiple concurrent audio objects into speaker channels
- Panning law energy normalization (e.g., constant-power $L_2$ normalization vs amplitude $L_1$ normalization)
- Headroom management
- Clipping detection and protection (e.g., float headroom, optional peak limiting / normalization pass)
- Loudness considerations and predictable gain staging

### Export & I/O (`src/io/`)
Must support:
- Standard **WAV** (RIFF, < 4 GB) and **RF64** (BWF/EBU Tech 3306, > 4 GB multichannel files)
- **PCM `f32`** (32-bit IEEE float) and **PCM `f64`** (64-bit IEEE float)
- Sample interleaving from per-channel buffers to output frames
- Streaming / chunked writes suitable for large multichannel renders without excessive memory bloat

---

## 8. Rust Engineering Standards

Priorities in strict order:
1. **Clarity** — straightforward types, explicit data flow, readable DSP math.
2. **Safety** — safe Rust by default, bounded indexing, checked arithmetic where overflow matters.
3. **Performance** — cache-friendly block processing, zero allocations in hot inner sample loops.
4. **Maintainability** — clean module boundaries, focused unit/integration tests.

**Rules:**
- Avoid unnecessary `unsafe` code.
- Avoid excessive or clever declarative/procedural macros.
- Avoid overengineering and premature abstraction.
- **Justify every external crate dependency** before adding it to `Cargo.toml` across these categories:
  - Linear algebra / 3D geometry
  - WAV / RF64 I/O
  - SIMD
  - Parallelization
  - Benchmarking
  - Testing

---

## 9. Forbidden Anti-Patterns

Do **NOT** introduce any of the following into this codebase:
- Entity Component Systems (ECS)
- Runtime plugin systems or dynamic ABI loading
- Dependency injection containers
- Service locators
- Unnecessary factory abstractions
- Generic "audio engine" frameworks

Keep the codebase pragmatic for a small team: plain structs, direct function calls, minimal traits only where polymorphism is genuinely needed, simple debugging, and rapid progress toward verified audio output.

---

## 10. Planned Project Structure

Target layout once implementation begins (subject to minor refinement during architecture review):

```text
src/
├── atmos/      # Atmos importer & metadata translation (isolated from core)
├── scene/      # Generic Scene, AudioObject, Keyframe intermediate model
├── layout/     # Arbitrary 3D speaker geometry & layout definitions
├── renderer/   # Spatial rendering / panning engine (VBAP, spread, etc.)
├── mixer/      # Multi-object mixing, headroom, clipping, normalization
├── timeline/   # Keyframe interpolation, automation, smoothing, crossfades
├── io/         # PCM bus, channel interleaving, WAV & RF64 writers
├── math/       # 3D vectors, spherical/Cartesian coords, triangulation helpers
└── tests/      # Integration & verification tests (or Cargo `tests/` dir)
```

---

## 11. Roadmap

- **Phase 1 (MVP)**: Generic `Scene`, arbitrary speaker layouts, single static object rendering, WAV export.
- **Phase 2**: Moving objects, keyframes, timeline interpolation and smoothing.
- **Phase 3**: Full 3D VBAP (including virtual speakers/hull handling as needed), object spread/width, energy normalization.
- **Phase 4**: SIMD optimizations for inner mixing and panning loops.
- **Phase 5**: Parallelization (e.g., object/block-level offline rendering concurrency).
- **Phase 6**: Advanced features (e.g., RF64 streaming, Atmos importer integration with `LibTrueHD`/`truehdd` outputs, loudness/limiting tools).

---

## 12. Current Status & Next Step (Architecture Phase)

**Current Stage**: Pre-implementation repository bootstrap. No Rust crate (`Cargo.toml`) or source code exists yet.

Before writing Phase 1 Rust code, the team must finalize and document the **10 architecture deliverables**:
1. Complete system architecture
2. Module dependency diagram
3. End-to-end data flow
4. Core Rust data structures
5. Core traits (kept minimal and purposeful)
6. Core rendering & panning pseudocode
7. Exact Phase 1 MVP specification
8. Phased implementation plan & crate selections
9. Verification & testing strategy (math invariants, energy conservation, golden WAV outputs)
10. Technical risks & mitigations

**Implementation Rule**: Start with the simplest possible working MVP capable of accurately rendering a scene containing a **single static object** to an **arbitrary speaker layout** as a **WAV** file. Only after that baseline is tested and verified should we evolve toward movement, automation, importer integration, and SIMD/parallel optimizations.
