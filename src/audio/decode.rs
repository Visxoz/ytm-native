//! Demuxes WebM or MP4 audio with Symphonia and decodes it to interleaved
//! f32 samples: AAC through Symphonia, Opus through `opus-decoder`.

use opus_decoder::OpusDecoder;
use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions, well_known::CODEC_ID_OPUS},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    units::Time,
};

/// Opus always decodes at 48 kHz.
const OPUS_RATE: u32 = 48_000;
/// The longest Opus frame is 120 ms.
const OPUS_MAX_FRAME: usize = 5_760;

enum Codec {
    Symphonia(Box<dyn AudioDecoder>),
    Opus {
        decoder: Box<OpusDecoder>,
        buf: Vec<f32>,
        /// Encoder delay still to drop from the start of the stream.
        pre_skip: usize,
    },
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    codec: Codec,
    track_id: u32,
    pub rate: u32,
    pub channels: usize,
}

pub enum Decoded {
    Samples,
    EndOfStream,
}

impl Decoder {
    pub fn open(reader: impl MediaSource + 'static, mime: &str) -> Result<Self, String> {
        let mut hint = Hint::new();
        hint.mime_type(mime.split(';').next().unwrap_or(mime).trim());
        let mss = MediaSourceStream::new(Box::new(reader), Default::default());
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| format!("unsupported stream: {e}"))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or("stream has no audio track")?;
        let track_id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("stream has no audio parameters")?
            .clone();
        let channels = params
            .channels
            .as_ref()
            .map_or(2, |c| c.count())
            .clamp(1, 2);

        let (codec, rate) = if params.codec == CODEC_ID_OPUS {
            let decoder = OpusDecoder::new(OPUS_RATE, channels)
                .map_err(|e| format!("Opus decoder: {e:?}"))?;
            let pre_skip = params
                .extra_data
                .as_deref()
                .and_then(opus_pre_skip)
                .unwrap_or(0);
            let codec = Codec::Opus {
                decoder: Box::new(decoder),
                buf: vec![0.0; OPUS_MAX_FRAME * channels],
                pre_skip,
            };
            (codec, OPUS_RATE)
        } else {
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(&params, &AudioDecoderOptions::default())
                .map_err(|e| format!("unsupported codec: {e}"))?;
            let rate = params.sample_rate.ok_or("stream has no sample rate")?;
            (Codec::Symphonia(decoder), rate)
        };

        Ok(Self {
            format,
            codec,
            track_id,
            rate,
            channels,
        })
    }

    /// Decodes the next packet, appending its samples to `out`.
    pub fn decode_next(&mut self, out: &mut Vec<f32>) -> Result<Decoded, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(Decoded::EndOfStream),
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(Decoded::EndOfStream);
                }
                Err(e) => return Err(format!("demux failed: {e}")),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match &mut self.codec {
                Codec::Symphonia(decoder) => match decoder.decode(&packet) {
                    Ok(buf) => {
                        let channels = buf.spec().channels().count();
                        let mut samples = Vec::new();
                        buf.copy_to_vec_interleaved::<f32>(&mut samples);
                        push_channels(out, &samples, channels, self.channels);
                    }
                    Err(SymphoniaError::DecodeError(e)) => {
                        log::debug!("skipped undecodable packet: {e}");
                        continue;
                    }
                    Err(e) => return Err(format!("decode failed: {e}")),
                },
                Codec::Opus {
                    decoder,
                    buf,
                    pre_skip,
                } => match decoder.decode_float(&packet.data, buf, false) {
                    Ok(frames) => {
                        let skip = (*pre_skip).min(frames);
                        *pre_skip -= skip;
                        out.extend_from_slice(&buf[skip * self.channels..frames * self.channels]);
                    }
                    Err(e) => {
                        log::debug!("skipped undecodable Opus packet: {e:?}");
                        continue;
                    }
                },
            }
            return Ok(Decoded::Samples);
        }
    }

    /// Seeks to `secs` and returns the position actually reached.
    pub fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let time = Time::try_from_secs_f64(secs.max(0.0)).ok_or("invalid seek time")?;
        let seeked = self
            .format
            .seek(
                SeekMode::Coarse,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("seek failed: {e}"))?;
        match &mut self.codec {
            Codec::Symphonia(decoder) => decoder.reset(),
            Codec::Opus {
                decoder, pre_skip, ..
            } => {
                decoder.reset();
                *pre_skip = 0;
            }
        }
        let reached = self
            .format
            .tracks()
            .iter()
            .find(|t| t.id == self.track_id)
            .and_then(|t| t.time_base)
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map_or(secs, |t| t.as_secs_f64());
        Ok(reached)
    }
}

/// Reads the pre-skip field of an OpusHead header.
fn opus_pre_skip(head: &[u8]) -> Option<usize> {
    if head.len() < 12 || &head[..8] != b"OpusHead" {
        return None;
    }
    Some(u16::from_le_bytes([head[10], head[11]]) as usize)
}

/// Appends `samples` (interleaved, `from` channels) as `to` channels.
fn push_channels(out: &mut Vec<f32>, samples: &[f32], from: usize, to: usize) {
    if from == to {
        out.extend_from_slice(samples);
        return;
    }
    for frame in samples.chunks_exact(from.max(1)) {
        match to {
            1 => out.push(frame.iter().sum::<f32>() / from as f32),
            _ => {
                let left = frame[0];
                let right = *frame.get(1).unwrap_or(&left);
                out.extend_from_slice(&[left, right]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_opus_pre_skip() {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        assert_eq!(opus_pre_skip(&head), Some(312));
        assert_eq!(opus_pre_skip(b"OpusTags"), None);
    }

    #[test]
    fn remaps_channels() {
        let mut out = Vec::new();
        push_channels(&mut out, &[0.5, 1.0], 1, 2);
        assert_eq!(out, [0.5, 0.5, 1.0, 1.0]);

        out.clear();
        push_channels(&mut out, &[0.2, 0.4, 0.6, 0.8], 2, 1);
        assert!((out[0] - 0.3).abs() < 1e-6 && (out[1] - 0.7).abs() < 1e-6);
    }
}
