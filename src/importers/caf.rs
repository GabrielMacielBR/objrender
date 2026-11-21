//! Streaming Apple Core Audio Format (CAF) LPCM reader for decoded multichannel audio.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::io::IoError;
use crate::scene::AudioSource;

const CACHE_BLOCK_FRAMES: usize = 4096;
const CAF_FORMAT_FLAG_FLOAT: u32 = 1 << 0;
const CAF_FORMAT_FLAG_LITTLE_ENDIAN: u32 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CafInfo {
    pub sample_rate: u32,
    pub num_channels: usize,
    pub bits_per_channel: u32,
    pub is_float: bool,
    pub is_little_endian: bool,
    pub bytes_per_frame: usize,
    pub data_offset: u64,
    pub total_frames: u64,
}

/// Parses a CAF header (`caff`, `desc`, `data`) from any seekable reader.
pub fn inspect_caf<R: Read + Seek>(reader: &mut R) -> Result<CafInfo, IoError> {
    reader.seek(SeekFrom::Start(0))?;
    let mut file_hdr = [0u8; 8];
    reader.read_exact(&mut file_hdr)?;
    if &file_hdr[0..4] != b"caff" {
        return Err(IoError::InvalidConfig(
            "not a CAF file (missing `caff` magic)".into(),
        ));
    }
    let version = u16::from_be_bytes([file_hdr[4], file_hdr[5]]);
    if version != 1 {
        return Err(IoError::InvalidConfig(format!(
            "unsupported CAF version {version}"
        )));
    }

    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(8))?;

    let mut desc: Option<(u32, usize, u32, bool, bool, usize)> = None;
    let mut data_bounds: Option<(u64, i64)> = None;

    while reader.stream_position()? + 12 <= file_len {
        let mut chunk_hdr = [0u8; 12];
        reader.read_exact(&mut chunk_hdr)?;
        let chunk_type: [u8; 4] = chunk_hdr[0..4].try_into().unwrap();
        let chunk_size = i64::from_be_bytes(chunk_hdr[4..12].try_into().unwrap());
        let body_start = reader.stream_position()?;

        match &chunk_type {
            b"desc" => {
                if chunk_size != 32 {
                    return Err(IoError::InvalidConfig(format!(
                        "unexpected CAF `desc` chunk size {chunk_size}"
                    )));
                }
                let mut buf = [0u8; 32];
                reader.read_exact(&mut buf)?;
                let sample_rate = f64::from_be_bytes(buf[0..8].try_into().unwrap());
                let format_id = &buf[8..12];
                if format_id != b"lpcm" {
                    return Err(IoError::InvalidConfig(format!(
                        "unsupported CAF format `{}` (expected `lpcm`)",
                        String::from_utf8_lossy(format_id)
                    )));
                }
                let format_flags = u32::from_be_bytes(buf[12..16].try_into().unwrap());
                let bytes_per_packet = u32::from_be_bytes(buf[16..20].try_into().unwrap()) as usize;
                let frames_per_packet = u32::from_be_bytes(buf[20..24].try_into().unwrap());
                let channels = u32::from_be_bytes(buf[24..28].try_into().unwrap()) as usize;
                let bits = u32::from_be_bytes(buf[28..32].try_into().unwrap());

                if frames_per_packet != 1 || channels == 0 || bytes_per_packet == 0 {
                    return Err(IoError::InvalidConfig(
                        "invalid CAF LPCM packet/channel geometry".into(),
                    ));
                }
                let is_float = (format_flags & CAF_FORMAT_FLAG_FLOAT) != 0;
                let is_le = (format_flags & CAF_FORMAT_FLAG_LITTLE_ENDIAN) != 0;
                let supported = matches!((is_float, bits), (false, 16 | 24 | 32) | (true, 32 | 64));
                if !supported {
                    return Err(IoError::InvalidConfig(format!(
                        "unsupported CAF LPCM sample format: float={is_float}, bits={bits}"
                    )));
                }
                if !sample_rate.is_finite() || sample_rate <= 0.0 {
                    return Err(IoError::InvalidConfig(format!(
                        "invalid CAF sample rate {sample_rate}"
                    )));
                }
                desc = Some((
                    sample_rate.round() as u32,
                    channels,
                    bits,
                    is_float,
                    is_le,
                    bytes_per_packet,
                ));
            }
            b"data" => {
                // First 4 bytes of CAF `data` chunk are mEditCount (u32 BE).
                if (0..4).contains(&chunk_size) {
                    return Err(IoError::InvalidConfig(
                        "CAF `data` chunk smaller than editCount".into(),
                    ));
                }
                let data_offset = body_start + 4;
                data_bounds = Some((data_offset, chunk_size));
                if chunk_size < 0 {
                    // -1 size means data runs until EOF.
                    break;
                }
                reader.seek(SeekFrom::Start(body_start + chunk_size as u64))?;
            }
            _ => {
                if chunk_size < 0 {
                    break;
                }
                reader.seek(SeekFrom::Start(body_start + chunk_size as u64))?;
            }
        }
    }

    let (sample_rate, num_channels, bits_per_channel, is_float, is_little_endian, bytes_per_frame) =
        desc.ok_or_else(|| IoError::InvalidConfig("CAF missing `desc` chunk".into()))?;
    let (data_offset, data_chunk_size) =
        data_bounds.ok_or_else(|| IoError::InvalidConfig("CAF missing `data` chunk".into()))?;

    let pcm_bytes = if data_chunk_size < 0 {
        file_len.saturating_sub(data_offset)
    } else {
        (data_chunk_size as u64)
            .saturating_sub(4)
            .min(file_len.saturating_sub(data_offset))
    };
    let total_frames = pcm_bytes / (bytes_per_frame as u64);

    Ok(CafInfo {
        sample_rate,
        num_channels,
        bits_per_channel,
        is_float,
        is_little_endian,
        bytes_per_frame,
        data_offset,
        total_frames,
    })
}

/// Shared seekable CAF stream with a planar block cache so multiple object channels
/// read sequential blocks with minimal disk seeks.
pub struct SharedCafStream<R: Read + Seek> {
    reader: R,
    info: CafInfo,
    cached_start: u64,
    cached_len: usize,
    planar_cache: Vec<Vec<f64>>,
    raw_buf: Vec<u8>,
}

impl SharedCafStream<File> {
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Mutex<Self>>, IoError> {
        let file = File::open(path)?;
        Self::from_reader(file)
    }
}

impl<R: Read + Seek> SharedCafStream<R> {
    pub fn from_reader(mut reader: R) -> Result<Arc<Mutex<Self>>, IoError> {
        let info = inspect_caf(&mut reader)?;
        let planar_cache = vec![vec![0.0; CACHE_BLOCK_FRAMES]; info.num_channels];
        let raw_buf = vec![0u8; CACHE_BLOCK_FRAMES * info.bytes_per_frame];
        Ok(Arc::new(Mutex::new(Self {
            reader,
            info,
            cached_start: 0,
            cached_len: 0,
            planar_cache,
            raw_buf,
        })))
    }

    pub fn info(&self) -> CafInfo {
        self.info
    }

    fn fill_cache(&mut self, start_frame: u64) -> Result<(), IoError> {
        if start_frame >= self.info.total_frames {
            self.cached_start = start_frame;
            self.cached_len = 0;
            return Ok(());
        }
        let avail = ((self.info.total_frames - start_frame) as usize).min(CACHE_BLOCK_FRAMES);
        let byte_offset = self.info.data_offset + start_frame * (self.info.bytes_per_frame as u64);
        let byte_count = avail * self.info.bytes_per_frame;

        self.reader.seek(SeekFrom::Start(byte_offset))?;
        self.reader.read_exact(&mut self.raw_buf[..byte_count])?;

        let n_ch = self.info.num_channels;
        let bps = self.info.bytes_per_frame / n_ch;
        for f in 0..avail {
            let frame_base = f * self.info.bytes_per_frame;
            for ch in 0..n_ch {
                let s_base = frame_base + ch * bps;
                let sample = decode_sample(
                    &self.raw_buf[s_base..s_base + bps],
                    self.info.is_float,
                    self.info.bits_per_channel,
                    self.info.is_little_endian,
                );
                self.planar_cache[ch][f] = sample;
            }
        }

        self.cached_start = start_frame;
        self.cached_len = avail;
        Ok(())
    }

    fn read_channel_frames(
        &mut self,
        channel: usize,
        mut frame_offset: u64,
        out: &mut [f64],
    ) -> Result<usize, IoError> {
        out.fill(0.0);
        if channel >= self.info.num_channels || frame_offset >= self.info.total_frames {
            return Ok(0);
        }

        let total_to_read = ((self.info.total_frames - frame_offset) as usize).min(out.len());
        let mut written = 0usize;

        while written < total_to_read {
            let cache_end = self.cached_start + self.cached_len as u64;
            if self.cached_len == 0 || frame_offset < self.cached_start || frame_offset >= cache_end
            {
                self.fill_cache(frame_offset)?;
                if self.cached_len == 0 {
                    break;
                }
            }

            let in_cache_offset = (frame_offset - self.cached_start) as usize;
            let copy_count = (self.cached_len - in_cache_offset).min(total_to_read - written);
            out[written..written + copy_count].copy_from_slice(
                &self.planar_cache[channel][in_cache_offset..in_cache_offset + copy_count],
            );

            written += copy_count;
            frame_offset += copy_count as u64;
        }

        Ok(written)
    }
}

/// Mono [`AudioSource`] backed by one channel of a [`SharedCafStream`].
pub struct CafChannelSource<R: Read + Seek + Send> {
    shared: Arc<Mutex<SharedCafStream<R>>>,
    channel: usize,
    total_frames: u64,
}

impl<R: Read + Seek + Send> CafChannelSource<R> {
    pub fn new(shared: Arc<Mutex<SharedCafStream<R>>>, channel: usize) -> Result<Self, IoError> {
        let info = shared
            .lock()
            .map_err(|_| IoError::InvalidConfig("CAF stream mutex poisoned".into()))?
            .info();
        if channel >= info.num_channels {
            return Err(IoError::InvalidConfig(format!(
                "CAF channel {channel} out of range ({} channels)",
                info.num_channels
            )));
        }
        Ok(Self {
            shared,
            channel,
            total_frames: info.total_frames,
        })
    }
}

impl<R: Read + Seek + Send> AudioSource for CafChannelSource<R> {
    fn total_frames(&self) -> u64 {
        self.total_frames
    }

    fn read_frames(&mut self, frame_offset: u64, out: &mut [f64]) -> Result<usize, IoError> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| IoError::InvalidConfig("CAF stream mutex poisoned".into()))?;
        guard.read_channel_frames(self.channel, frame_offset, out)
    }
}

#[inline]
fn decode_sample(bytes: &[u8], is_float: bool, bits: u32, is_le: bool) -> f64 {
    match (is_float, bits, is_le) {
        (false, 16, true) => i16::from_le_bytes([bytes[0], bytes[1]]) as f64 / 32_768.0,
        (false, 16, false) => i16::from_be_bytes([bytes[0], bytes[1]]) as f64 / 32_768.0,
        (false, 24, true) => {
            let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
            ((raw << 8) >> 8) as f64 / 8_388_608.0
        }
        (false, 24, false) => {
            let raw = ((bytes[0] as i32) << 16) | ((bytes[1] as i32) << 8) | (bytes[2] as i32);
            ((raw << 8) >> 8) as f64 / 8_388_608.0
        }
        (false, 32, true) => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64 / 2_147_483_648.0
        }
        (false, 32, false) => {
            i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64 / 2_147_483_648.0
        }
        (true, 32, true) => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
        (true, 32, false) => f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
        (true, 64, true) => f64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        (true, 64, false) => f64::from_be_bytes(bytes[0..8].try_into().unwrap()),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use std::io::Cursor;

    pub(crate) fn build_test_caf(
        sample_rate: u32,
        num_channels: u32,
        bits: u32,
        is_float: bool,
        is_le: bool,
        eof_sized: bool,
        pcm_payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"caff");
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        // desc chunk
        out.extend_from_slice(b"desc");
        out.extend_from_slice(&32i64.to_be_bytes());
        out.extend_from_slice(&(sample_rate as f64).to_be_bytes());
        out.extend_from_slice(b"lpcm");
        let flags = (if is_float { CAF_FORMAT_FLAG_FLOAT } else { 0 })
            | (if is_le {
                CAF_FORMAT_FLAG_LITTLE_ENDIAN
            } else {
                0
            });
        out.extend_from_slice(&flags.to_be_bytes());
        let bytes_per_packet = num_channels * (bits / 8);
        out.extend_from_slice(&bytes_per_packet.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&num_channels.to_be_bytes());
        out.extend_from_slice(&bits.to_be_bytes());

        // data chunk
        out.extend_from_slice(b"data");
        let chunk_size: i64 = if eof_sized {
            -1
        } else {
            4 + pcm_payload.len() as i64
        };
        out.extend_from_slice(&chunk_size.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // editCount
        out.extend_from_slice(pcm_payload);
        out
    }

    #[test]
    fn decodes_24bit_le_and_be_and_eof_sized_caf() {
        // 2 channels, 2 frames, 24-bit LE: [+0.5, -0.5], [+0.25, -1.0]
        let s_pos_half: i32 = 4_194_304; // 0.5 * 2^23
        let s_neg_half: i32 = -4_194_304;
        let s_pos_quarter: i32 = 2_097_152;
        let s_neg_one: i32 = -8_388_608;

        let mut le_pcm = Vec::new();
        for v in [s_pos_half, s_neg_half, s_pos_quarter, s_neg_one] {
            let b = v.to_le_bytes();
            le_pcm.extend_from_slice(&b[0..3]);
        }
        let caf_le = build_test_caf(48_000, 2, 24, false, true, true, &le_pcm);
        let shared = SharedCafStream::from_reader(Cursor::new(caf_le)).unwrap();
        let mut ch0 = CafChannelSource::new(Arc::clone(&shared), 0).unwrap();
        let mut ch1 = CafChannelSource::new(shared, 1).unwrap();
        assert_eq!(ch0.total_frames(), 2);

        let mut buf0 = [0.0; 4];
        let mut buf1 = [0.0; 4];
        assert_eq!(ch0.read_frames(0, &mut buf0).unwrap(), 2);
        assert_eq!(ch1.read_frames(0, &mut buf1).unwrap(), 2);
        assert_relative_eq!(buf0[0], 0.5, epsilon = 1e-6);
        assert_relative_eq!(buf0[1], 0.25, epsilon = 1e-6);
        assert_relative_eq!(buf0[2], 0.0, epsilon = 1e-12);
        assert_relative_eq!(buf1[0], -0.5, epsilon = 1e-6);
        assert_relative_eq!(buf1[1], -1.0, epsilon = 1e-6);

        // 24-bit BE single sample
        let b = s_pos_half.to_be_bytes();
        let caf_be = build_test_caf(48_000, 1, 24, false, false, false, &b[1..4]);
        let shared_be = SharedCafStream::from_reader(Cursor::new(caf_be)).unwrap();
        let mut src_be = CafChannelSource::new(shared_be, 0).unwrap();
        let mut out_be = [0.0; 1];
        src_be.read_frames(0, &mut out_be).unwrap();
        assert_relative_eq!(out_be[0], 0.5, epsilon = 1e-6);
    }

    #[test]
    fn decodes_f32_le_caf() {
        let mut pcm = Vec::new();
        for v in [-0.75_f32, 0.125_f32] {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let caf = build_test_caf(96_000, 1, 32, true, true, false, &pcm);
        let shared = SharedCafStream::from_reader(Cursor::new(caf)).unwrap();
        assert_eq!(shared.lock().unwrap().info().sample_rate, 96_000);
        let mut src = CafChannelSource::new(shared, 0).unwrap();
        let mut out = [0.0; 2];
        src.read_frames(0, &mut out).unwrap();
        assert_relative_eq!(out[0], -0.75, epsilon = 1e-7);
        assert_relative_eq!(out[1], 0.125, epsilon = 1e-7);
    }
}
