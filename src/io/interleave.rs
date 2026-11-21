//! Planar `f64` to interleaved little-endian byte packers with `pulp` runtime dispatch.

use pulp::{Arch, Simd, WithSimd};

use crate::io::PcmBus;

const TILE_FRAMES: usize = 8;
const TILE_CHANNELS: usize = 16;

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
