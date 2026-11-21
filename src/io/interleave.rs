//! Planar `f64` to interleaved little-endian byte packers with `pulp` runtime dispatch.

use pulp::{Arch, Simd, WithSimd};

use crate::io::{PcmBus, SampleFormat};

const TILE_FRAMES: usize = 8;
const TILE_CHANNELS: usize = 16;

/// Deterministic SplitMix64 triangular-PDF (TPDF) dither generator in `(-1.0, +1.0)` LSB.
#[derive(Debug, Clone)]
pub struct TpdfDither {
    state: u64,
}

impl Default for TpdfDither {
    fn default() -> Self {
        Self::with_seed(0x9E37_79B9_7F4A_7C15)
    }
}

impl TpdfDither {
    pub const fn with_seed(seed: u64) -> Self {
        let s = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state: s }
    }

    #[inline]
    fn next_uniform(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // 53-bit uniform mantissa in [0.0, 1.0).
        ((z >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// Returns a zero-mean TPDF deviate in `(-1.0, +1.0)` LSB.
    #[inline]
    pub fn next_lsb(&mut self) -> f64 {
        self.next_uniform() - self.next_uniform()
    }
}

/// Packs planar `f64` frames into interleaved little-endian bytes for any supported [`SampleFormat`].
pub fn pack_bus_le(
    arch: Arch,
    format: SampleFormat,
    bus: &PcmBus,
    frames: usize,
    dither: Option<&mut TpdfDither>,
    out: &mut Vec<u8>,
) {
    match format {
        SampleFormat::Int16 => interleave_i16_le(bus, frames, dither, out),
        SampleFormat::Int24 => interleave_i24_le(bus, frames, dither, out),
        SampleFormat::Float32 => interleave_f32_le_simd(arch, bus, frames, out),
        SampleFormat::Float64 => interleave_f64_le_simd(arch, bus, frames, out),
    }
}

/// Planar-to-interleaved signed 16-bit PCM little-endian packer with optional TPDF dither.
pub fn interleave_i16_le(
    bus: &PcmBus,
    frames: usize,
    mut dither: Option<&mut TpdfDither>,
    out: &mut Vec<u8>,
) {
    out.clear();
    let n_ch = bus.num_channels();
    if frames == 0 || n_ch == 0 {
        return;
    }
    let needed = frames * n_ch * 2;
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }
    let channels = bus.channels();
    for s in 0..frames {
        for ch_vec in channels {
            let v = ch_vec[s];
            let scaled = if v.is_finite() { v * 32_768.0 } else { 0.0 };
            let noise = match dither.as_deref_mut() {
                Some(rng) => rng.next_lsb(),
                None => 0.0,
            };
            let q = (scaled + noise).round().clamp(-32_768.0, 32_767.0) as i16;
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
}

/// Planar-to-interleaved signed 24-bit PCM little-endian packer with optional TPDF dither.
pub fn interleave_i24_le(
    bus: &PcmBus,
    frames: usize,
    mut dither: Option<&mut TpdfDither>,
    out: &mut Vec<u8>,
) {
    out.clear();
    let n_ch = bus.num_channels();
    if frames == 0 || n_ch == 0 {
        return;
    }
    let needed = frames * n_ch * 3;
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }
    let channels = bus.channels();
    for s in 0..frames {
        for ch_vec in channels {
            let v = ch_vec[s];
            let scaled = if v.is_finite() { v * 8_388_608.0 } else { 0.0 };
            let noise = match dither.as_deref_mut() {
                Some(rng) => rng.next_lsb(),
                None => 0.0,
            };
            let q = (scaled + noise).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
            let bytes = q.to_le_bytes();
            out.extend_from_slice(&bytes[0..3]);
        }
    }
}

/// Scalar reference planar-to-interleaved `f32` little-endian packer.
pub fn interleave_f32_le_scalar(bus: &PcmBus, frames: usize, out: &mut Vec<u8>) {
    out.clear();
    if frames == 0 || bus.num_channels() == 0 {
        return;
    }
    let n_ch = bus.num_channels();
    let needed = frames * n_ch * 4;
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }
    for s in 0..frames {
        for ch in 0..n_ch {
            let sample = bus.channel(ch)[s] as f32;
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
}

/// Scalar reference planar-to-interleaved `f64` little-endian packer.
pub fn interleave_f64_le_scalar(bus: &PcmBus, frames: usize, out: &mut Vec<u8>) {
    out.clear();
    if frames == 0 || bus.num_channels() == 0 {
        return;
    }
    let n_ch = bus.num_channels();
    let needed = frames * n_ch * 8;
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }
    for s in 0..frames {
        for ch in 0..n_ch {
            let sample = bus.channel(ch)[s];
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
}

/// SIMD-dispatched tiled planar-to-interleaved `f32` little-endian packer.
pub fn interleave_f32_le_simd(arch: Arch, bus: &PcmBus, frames: usize, out: &mut Vec<u8>) {
    let n_ch = bus.num_channels();
    if frames == 0 || n_ch == 0 {
        out.clear();
        return;
    }
    out.resize(frames * n_ch * 4, 0);
    arch.dispatch(InterleaveF32 {
        channels: bus.channels(),
        frames,
        out,
    });
}

/// SIMD-dispatched tiled planar-to-interleaved `f64` little-endian packer.
pub fn interleave_f64_le_simd(arch: Arch, bus: &PcmBus, frames: usize, out: &mut Vec<u8>) {
    let n_ch = bus.num_channels();
    if frames == 0 || n_ch == 0 {
        out.clear();
        return;
    }
    out.resize(frames * n_ch * 8, 0);
    arch.dispatch(InterleaveF64 {
        channels: bus.channels(),
        frames,
        out,
    });
}

struct InterleaveF32<'a> {
    channels: &'a [Vec<f64>],
    frames: usize,
    out: &'a mut [u8],
}

impl WithSimd for InterleaveF32<'_> {
    type Output = ();

    #[inline(always)]
    fn with_simd<S: Simd>(self, _simd: S) {
        let n_ch = self.channels.len();
        let stride = n_ch * 4;
        let full_tiles = self.frames / TILE_FRAMES;
        let tiled_frames = full_tiles * TILE_FRAMES;
        let mut tile = [[0.0_f32; TILE_FRAMES]; TILE_CHANNELS];

        for t in 0..full_tiles {
            let f0 = t * TILE_FRAMES;
            let out_tile = &mut self.out[f0 * stride..(f0 + TILE_FRAMES) * stride];

            for ch_base in (0..n_ch).step_by(TILE_CHANNELS) {
                let count = (n_ch - ch_base).min(TILE_CHANNELS);
                for (local_ch, ch_vec) in self.channels[ch_base..ch_base + count].iter().enumerate()
                {
                    let src = &ch_vec[f0..f0 + TILE_FRAMES];
                    for (dst, &v) in tile[local_ch].iter_mut().zip(src) {
                        *dst = v as f32;
                    }
                }
                for (k, frame_bytes) in out_tile.chunks_exact_mut(stride).enumerate() {
                    let dst_slice = &mut frame_bytes[ch_base * 4..(ch_base + count) * 4];
                    let (dst_chunks, _) = dst_slice.as_chunks_mut::<4>();
                    for (slot, ch_tile) in dst_chunks.iter_mut().zip(&tile[..count]) {
                        *slot = ch_tile[k].to_le_bytes();
                    }
                }
            }
        }

        for s in tiled_frames..self.frames {
            let frame_bytes = &mut self.out[s * stride..(s + 1) * stride];
            let (dst_chunks, _) = frame_bytes.as_chunks_mut::<4>();
            for (slot, ch_vec) in dst_chunks.iter_mut().zip(self.channels) {
                let v = ch_vec[s] as f32;
                *slot = v.to_le_bytes();
            }
        }
    }
}

struct InterleaveF64<'a> {
    channels: &'a [Vec<f64>],
    frames: usize,
    out: &'a mut [u8],
}

impl WithSimd for InterleaveF64<'_> {
    type Output = ();

    #[inline(always)]
    fn with_simd<S: Simd>(self, _simd: S) {
        let n_ch = self.channels.len();
        let stride = n_ch * 8;
        let full_tiles = self.frames / TILE_FRAMES;
        let tiled_frames = full_tiles * TILE_FRAMES;
        let mut tile = [[0.0_f64; TILE_FRAMES]; TILE_CHANNELS];

        for t in 0..full_tiles {
            let f0 = t * TILE_FRAMES;
            let out_tile = &mut self.out[f0 * stride..(f0 + TILE_FRAMES) * stride];

            for ch_base in (0..n_ch).step_by(TILE_CHANNELS) {
                let count = (n_ch - ch_base).min(TILE_CHANNELS);
                for (local_ch, ch_vec) in self.channels[ch_base..ch_base + count].iter().enumerate()
                {
                    tile[local_ch].copy_from_slice(&ch_vec[f0..f0 + TILE_FRAMES]);
                }
                for (k, frame_bytes) in out_tile.chunks_exact_mut(stride).enumerate() {
                    let dst_slice = &mut frame_bytes[ch_base * 8..(ch_base + count) * 8];
                    let (dst_chunks, _) = dst_slice.as_chunks_mut::<8>();
                    for (slot, ch_tile) in dst_chunks.iter_mut().zip(&tile[..count]) {
                        *slot = ch_tile[k].to_le_bytes();
                    }
                }
            }
        }

        for s in tiled_frames..self.frames {
            let frame_bytes = &mut self.out[s * stride..(s + 1) * stride];
            let (dst_chunks, _) = frame_bytes.as_chunks_mut::<8>();
            for (slot, ch_vec) in dst_chunks.iter_mut().zip(self.channels) {
                *slot = ch_vec[s].to_le_bytes();
            }
        }
    }
}
