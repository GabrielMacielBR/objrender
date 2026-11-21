//! Streaming WAV / RF64 (EBU Tech 3306) writer with automatic JUNK -> ds64 promotion.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use pulp::Arch;

use crate::io::interleave::{interleave_f32_le_simd, interleave_f64_le_simd};
use crate::io::{IoError, OutputMetadata, PcmBus, SampleFormat, SampleWriter};

const HEADER_BYTES: u64 = 104; // 12 (RIFF) + 36 (JUNK/ds64) + 48 (fmt ) + 8 (data hdr)
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: 00000003-0000-0010-8000-00aa00389b71
const SUBFORMAT_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

#[derive(Debug, Clone, Copy)]
pub struct WaveConfig {
    pub sample_rate: u32,
    pub num_channels: u16,
    pub channel_mask: u32,
    pub format: SampleFormat,
    /// Size threshold (bytes) above which RIFF promotes to RF64 `ds64` on `finalize()`.
    /// Defaults to `u32::MAX as u64` (~4 GiB). Can be lowered in tests.
    pub rf64_threshold_bytes: u64,
}

impl WaveConfig {
    pub fn new(
        sample_rate: u32,
        num_channels: u16,
        channel_mask: u32,
        format: SampleFormat,
    ) -> Self {
        Self {
            sample_rate,
            num_channels,
            channel_mask,
            format,
            rf64_threshold_bytes: u32::MAX as u64,
        }
    }
}

/// Streaming multichannel IEEE float WAV writer supporting automatic O(1) RF64 promotion.
pub struct WaveWriter<W: Write + Seek> {
    writer: BufWriter<W>,
    config: WaveConfig,
    frames_written: u64,
    scratch_bytes: Vec<u8>,
    arch: Arch,
}

impl WaveWriter<File> {
    pub fn create(path: impl AsRef<Path>, config: WaveConfig) -> Result<Self, IoError> {
        let file = File::create(path)?;
        Self::from_writer(file, config)
    }
}

impl<W: Write + Seek> WaveWriter<W> {
    pub fn from_writer(writer: W, config: WaveConfig) -> Result<Self, IoError> {
        if config.sample_rate == 0 || config.num_channels == 0 {
            return Err(IoError::InvalidConfig(
                "sample_rate and num_channels must be non-zero".to_string(),
            ));
        }

        let mut buf = BufWriter::with_capacity(64 * 1024, writer);
        write_placeholder_header(&mut buf, config)?;

        Ok(Self {
            writer: buf,
            config,
            frames_written: 0,
            scratch_bytes: Vec::new(),
            arch: Arch::new(),
        })
    }
}

impl<W: Write + Seek> SampleWriter for WaveWriter<W> {
    fn write_block(&mut self, bus: &PcmBus, frames: usize) -> Result<(), IoError> {
        let n_ch = self.config.num_channels as usize;
        if bus.num_channels() != n_ch {
            return Err(IoError::InvalidConfig(format!(
                "bus channels ({}) != wave channels ({})",
                bus.num_channels(),
                n_ch
            )));
        }
        if frames == 0 {
            return Ok(());
        }

        match self.config.format {
            SampleFormat::Float32 => {
                interleave_f32_le_simd(self.arch, bus, frames, &mut self.scratch_bytes);
            }
            SampleFormat::Float64 => {
                interleave_f64_le_simd(self.arch, bus, frames, &mut self.scratch_bytes);
            }
        }

        self.writer.write_all(&self.scratch_bytes)?;
        self.frames_written += frames as u64;
        Ok(())
    }

    fn finalize(mut self: Box<Self>) -> Result<OutputMetadata, IoError> {
        self.writer.flush()?;
        let mut inner = self
            .writer
            .into_inner()
            .map_err(|e| IoError::StdIo(e.into_error()))?;

        let frame_bytes =
            (self.config.num_channels as u64) * (self.config.format.bytes_per_sample() as u64);
        let data_bytes = self.frames_written * frame_bytes;
        let pad_byte = data_bytes % 2;
        let total_bytes = HEADER_BYTES + data_bytes + pad_byte;
        if pad_byte == 1 {
            inner.write_all(&[0u8])?;
        }
        let riff_size = total_bytes - 8;

        let needs_rf64 = riff_size > self.config.rf64_threshold_bytes
            || data_bytes > self.config.rf64_threshold_bytes;

        if !needs_rf64 {
            inner.seek(SeekFrom::Start(4))?;
            inner.write_all(&(riff_size as u32).to_le_bytes())?;
            inner.seek(SeekFrom::Start(HEADER_BYTES - 4))?;
            inner.write_all(&(data_bytes as u32).to_le_bytes())?;
        } else {
            inner.seek(SeekFrom::Start(0))?;
            inner.write_all(b"RF64")?;
            inner.write_all(&u32::MAX.to_le_bytes())?;

            // Overwrite reserved 36-byte JUNK chunk at offset 12 with ds64 (8 + 28 bytes).
            inner.seek(SeekFrom::Start(12))?;
            inner.write_all(b"ds64")?;
            inner.write_all(&28u32.to_le_bytes())?;
            inner.write_all(&riff_size.to_le_bytes())?;
            inner.write_all(&data_bytes.to_le_bytes())?;
            inner.write_all(&self.frames_written.to_le_bytes())?;
            inner.write_all(&0u32.to_le_bytes())?; // tableLength = 0

            inner.seek(SeekFrom::Start(HEADER_BYTES - 4))?;
            inner.write_all(&u32::MAX.to_le_bytes())?;
        }

        inner.flush()?;
        Ok(OutputMetadata {
            rf64_promoted: needs_rf64,
            frames_written: self.frames_written,
            total_bytes,
        })
    }
}

fn write_placeholder_header<W: Write>(w: &mut W, config: WaveConfig) -> Result<(), IoError> {
    let bps = config.format.bytes_per_sample();
    let bits = config.format.bits_per_sample();
    let block_align = config.num_channels * bps;
    let byte_rate = config.sample_rate * (block_align as u32);

    // 1. RIFF/WAVE header (12 bytes)
    w.write_all(b"RIFF")?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(b"WAVE")?;

    // 2. Reserved JUNK chunk (8 + 28 = 36 bytes) matching ds64 size for O(1) RF64 promotion
    w.write_all(b"JUNK")?;
    w.write_all(&28u32.to_le_bytes())?;
    w.write_all(&[0u8; 28])?;

    // 3. fmt chunk using WAVE_FORMAT_EXTENSIBLE (8 + 40 = 48 bytes)
    w.write_all(b"fmt ")?;
    w.write_all(&40u32.to_le_bytes())?;
    w.write_all(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes())?;
    w.write_all(&config.num_channels.to_le_bytes())?;
    w.write_all(&config.sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&bits.to_le_bytes())?;
    w.write_all(&22u16.to_le_bytes())?; // cbSize
    w.write_all(&bits.to_le_bytes())?; // wValidBitsPerSample
    w.write_all(&config.channel_mask.to_le_bytes())?;
    w.write_all(&SUBFORMAT_IEEE_FLOAT)?;

    // 4. data chunk header (8 bytes)
    w.write_all(b"data")?;
    w.write_all(&0u32.to_le_bytes())?;
    Ok(())
}
