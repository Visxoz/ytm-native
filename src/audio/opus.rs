//! A safe handle on libopus's decoder, from the Rust translation of the
//! reference implementation.

use unsafe_libopus::{
    OpusDecoder as RawDecoder, opus_decode_float, opus_decoder_create, opus_decoder_destroy,
};

/// The longest Opus frame is 120 ms at 48 kHz.
pub const MAX_FRAME: usize = 5_760;

pub struct OpusDecoder {
    raw: *mut RawDecoder,
    rate: u32,
    channels: usize,
}

// SAFETY: the decoder state is plain memory owned by this handle and is only
// touched through `&mut self`.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(rate: u32, channels: usize) -> Result<Self, String> {
        let mut error = 0;
        // SAFETY: `error` outlives the call; libopus validates its arguments.
        let raw = unsafe { opus_decoder_create(rate as i32, channels as i32, &mut error) };
        if raw.is_null() || error != 0 {
            return Err(format!(
                "libopus could not create a decoder (error {error})"
            ));
        }
        Ok(Self {
            raw,
            rate,
            channels,
        })
    }

    /// Decodes one packet into `pcm` (interleaved), returning frames written.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, String> {
        let frame_size = (pcm.len() / self.channels).min(MAX_FRAME) as i32;
        // SAFETY: `packet` and `pcm` are valid for the lengths passed, and
        // `pcm` holds `frame_size * channels` samples.
        let frames = unsafe {
            opus_decode_float(
                self.raw,
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                frame_size,
                0,
            )
        };
        if frames < 0 {
            return Err(format!("libopus error {frames}"));
        }
        Ok(frames as usize)
    }

    /// Clears the decoder's history, as after a seek.
    pub fn reset(&mut self) -> Result<(), String> {
        *self = Self::new(self.rate, self.channels)?;
        Ok(())
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `opus_decoder_create` and is freed once.
        unsafe { opus_decoder_destroy(self.raw) };
    }
}
