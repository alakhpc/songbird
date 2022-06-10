use crate::constants::*;
use audiopus::{
    coder::{Decoder as AudiopusDecoder, GenericCtl},
    Channels,
    Error as OpusError,
};
use symphonia_core::{
    audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Layout, Signal, SignalSpec},
    codecs::{
        CodecDescriptor,
        CodecParameters,
        Decoder,
        DecoderOptions,
        FinalizeResult,
        CODEC_TYPE_OPUS,
    },
    errors::{decode_error, Result as SymphResult},
    formats::Packet,
};

/// Opus decoder for symphonia, based on libopus v1.3 (via [`audiopus`]).
pub struct OpusDecoder {
    inner: AudiopusDecoder,
    params: CodecParameters,
    buf: AudioBuffer<f32>,
    rawbuf: Vec<f32>,
}

unsafe impl Sync for OpusDecoder {}

impl OpusDecoder {
    fn decode_inner(&mut self, packet: &Packet) -> SymphResult<()> {
        let s_ct = loop {
            let pkt = if packet.buf().is_empty() {
                None
            } else if let Ok(checked_pkt) = packet.buf().try_into() {
                Some(checked_pkt)
            } else {
                return decode_error("Opus packet was too large (greater than i32::MAX bytes).");
            };
            let out_space = (&mut self.rawbuf[..]).try_into().expect("The following logic expands this buffer safely below i32::MAX, and we throw our own error.");

            match self.inner.decode_float(pkt, out_space, false) {
                Ok(v) => break v,
                Err(OpusError::Opus(audiopus::ErrorCode::BufferTooSmall)) => {
                    // double the buffer size
                    // correct behav would be to mirror the decoder logic in the udp_rx set.
                    let new_size = (self.rawbuf.len() * 2).min(std::i32::MAX as usize);
                    if new_size == self.rawbuf.len() {
                        return decode_error("Opus frame too big: cannot expand opus frame decode buffer any further.");
                    }

                    self.rawbuf.resize(new_size, 0.0);
                    self.buf = AudioBuffer::new(
                        self.rawbuf.len() as u64 / 2,
                        SignalSpec::new_with_layout(SAMPLE_RATE_RAW as u32, Layout::Stereo),
                    );
                },
                Err(e) => {
                    tracing::error!("Opus decode error: {:?}", e);
                    return decode_error("Opus decode error: see 'tracing' logs.");
                },
            }
        };

        self.buf.clear();
        self.buf.render_reserved(Some(s_ct));

        // Forcibly assuming stereo, for now.
        for ch in 0..2 {
            let iter = self.rawbuf.chunks_exact(2).map(|chunk| chunk[ch]);
            for (tgt, src) in self.buf.chan_mut(ch).iter_mut().zip(iter) {
                *tgt = src;
            }
        }

        Ok(())
    }
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> SymphResult<Self> {
        let inner = AudiopusDecoder::new(SAMPLE_RATE, Channels::Stereo).unwrap();

        Ok(Self {
            inner,
            params: params.clone(),
            buf: AudioBuffer::new(
                MONO_FRAME_SIZE as u64,
                SignalSpec::new_with_layout(SAMPLE_RATE_RAW as u32, Layout::Stereo),
            ),
            rawbuf: vec![0.0f32; STEREO_FRAME_SIZE],
        })
    }

    fn supported_codecs() -> &'static [symphonia::core::codecs::CodecDescriptor] {
        &[symphonia_core::support_codec!(
            CODEC_TYPE_OPUS,
            "opus",
            "libopus (1.3+, audiopus)"
        )]
    }

    fn codec_params(&self) -> &symphonia::core::codecs::CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> SymphResult<AudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        } else {
            Ok(self.buf.as_audio_buffer_ref())
        }
    }

    fn reset(&mut self) {
        let _ = self.inner.reset_state();
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef {
        self.buf.as_audio_buffer_ref()
    }
}
