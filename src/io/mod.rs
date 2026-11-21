//! Planar mixing bus, sample format conversions, and streaming output writer traits.

pub mod wave;

use thiserror::Error;

pub use wave::{WaveConfig, WaveWriter};

#[derive(Debug, Error)]
pub enum IoError {
    #[error("I/O failure: {0}")]
    StdIo(#[from] std::io::Error),
    #[error("invalid wave configuration: {0}")]
    InvalidConfig(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Float32,
    Float64,
}

impl SampleFormat {
    #[inline]
    pub const fn bytes_per_sample(self) -> u16 {
        match self {
            Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }

    #[inline]
    pub const fn bits_per_sample(self) -> u16 {
        self.bytes_per_sample() * 8
    }
}

/// Planar 64-bit floating-point mixing buffer (`channels[ch][sample]`).
#[derive(Debug, Clone)]
pub struct PcmBus {
    channels: Vec<Vec<f64>>,
    block_size: usize,
}

impl PcmBus {
    pub fn new(num_channels: usize, block_size: usize) -> Self {
        Self {
            channels: vec![vec![0.0; block_size]; num_channels],
            block_size,
        }
    }

    #[inline]
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    #[inline]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn clear(&mut self, frames: usize) {
        let n = frames.min(self.block_size);
        for ch in &mut self.channels {
            ch[..n].fill(0.0);
        }
    }

    #[inline]
    pub fn channel(&self, ch: usize) -> &[f64] {
        &self.channels[ch]
    }

    #[inline]
    pub fn channel_mut(&mut self, ch: usize) -> &mut [f64] {
        &mut self.channels[ch]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputMetadata {
    pub rf64_promoted: bool,
    pub frames_written: u64,
    pub total_bytes: u64,
}

/// Output sink consuming planar [`PcmBus`] blocks and finalizing container headers.
pub trait SampleWriter {
    fn write_block(&mut self, bus: &PcmBus, frames: usize) -> Result<(), IoError>;
    fn finalize(self: Box<Self>) -> Result<OutputMetadata, IoError>;
}

/// In-memory output sink capturing planar channels for unit tests.
#[derive(Debug, Default)]
pub struct MemorySink {
    pub channels: Vec<Vec<f64>>,
    pub frames_written: u64,
}

impl MemorySink {
    pub fn new(num_channels: usize) -> Self {
        Self {
            channels: vec![Vec::new(); num_channels],
            frames_written: 0,
        }
    }
}

impl SampleWriter for MemorySink {
    fn write_block(&mut self, bus: &PcmBus, frames: usize) -> Result<(), IoError> {
        if bus.num_channels() != self.channels.len() {
            return Err(IoError::InvalidConfig(format!(
                "bus channels ({}) != sink channels ({})",
                bus.num_channels(),
                self.channels.len()
            )));
        }
        for (ch_idx, dst) in self.channels.iter_mut().enumerate() {
            dst.extend_from_slice(&bus.channel(ch_idx)[..frames]);
        }
        self.frames_written += frames as u64;
        Ok(())
    }

    fn finalize(self: Box<Self>) -> Result<OutputMetadata, IoError> {
        Ok(OutputMetadata {
            rf64_promoted: false,
            frames_written: self.frames_written,
            total_bytes: 0,
        })
    }
}

/// Zero-allocation no-op sink used during the analysis pass of two-pass rendering.
#[derive(Debug, Default)]
pub struct NullWriter {
    frames_written: u64,
}

impl SampleWriter for NullWriter {
    fn write_block(&mut self, _bus: &PcmBus, frames: usize) -> Result<(), IoError> {
        self.frames_written += frames as u64;
        Ok(())
    }

    fn finalize(self: Box<Self>) -> Result<OutputMetadata, IoError> {
        Ok(OutputMetadata {
            rf64_promoted: false,
            frames_written: self.frames_written,
            total_bytes: 0,
        })
    }
}
