pub mod mix_logic;
mod pool;
mod result;
pub mod state;
pub mod track;
mod util;

use pool::*;
use result::*;
use state::*;
pub use track::*;

use super::{disposal, error::Result, message::*};
use crate::{
    constants::*,
    driver::{
        test_config::{OutputMessage, OutputMode, TickStyle},
        MixMode,
    },
    events::EventStore,
    input::{Input, Parsed},
    tracks::{Action, LoopState, PlayError, PlayMode, TrackCommand, TrackHandle, TrackState, View},
    Config,
};
use audiopus::{
    coder::Encoder as OpusEncoder,
    softclip::SoftClip,
    Application as CodingMode,
    Bitrate,
};
use discortp::{
    rtp::{MutableRtpPacket, RtpPacket},
    MutablePacket,
    Packet,
};
use flume::{Receiver, Sender, TryRecvError};
use rand::random;
use rubato::{FftFixedOut, Resampler};
use std::{
    io::Write,
    result::Result as StdResult,
    sync::Arc,
    time::{Duration, Instant},
};
use symphonia_core::{
    audio::{AudioBuffer, AudioBufferRef, Layout, SampleBuffer, Signal, SignalSpec},
    codecs::CODEC_TYPE_OPUS,
    conv::IntoSample,
    formats::SeekTo,
    sample::Sample,
    units::Time,
};
use tokio::runtime::Handle;
use tracing::{debug, error, instrument, warn};
use xsalsa20poly1305::TAG_SIZE;

pub struct Mixer {
    pub bitrate: Bitrate,
    pub config: Arc<Config>,
    pub conn_active: Option<MixerConnection>,
    pub content_prep_sequence: u64,
    pub deadline: Instant,
    pub disposer: Sender<DisposalMessage>,
    pub encoder: OpusEncoder,
    pub interconnect: Interconnect,
    pub mix_rx: Receiver<MixerMessage>,
    pub muted: bool,
    pub packet: [u8; VOICE_PACKET_MAX],
    pub prevent_events: bool,
    pub silence_frames: u8,
    pub skip_sleep: bool,
    pub soft_clip: SoftClip,
    thread_pool: BlockyTaskPool,
    pub ws: Option<Sender<WsMessage>>,

    pub tracks: Vec<InternalTrack>,
    track_handles: Vec<TrackHandle>,

    sample_buffer: SampleBuffer<f32>,
    symph_mix: AudioBuffer<f32>,
    resample_scratch: AudioBuffer<f32>,

    #[cfg(test)]
    remaining_loops: Option<u64>,
}

fn new_encoder(bitrate: Bitrate, mix_mode: MixMode) -> Result<OpusEncoder> {
    let mut encoder = OpusEncoder::new(SAMPLE_RATE, mix_mode.to_opus(), CodingMode::Audio)?;
    encoder.set_bitrate(bitrate)?;

    Ok(encoder)
}

impl Mixer {
    pub fn new(
        mix_rx: Receiver<MixerMessage>,
        async_handle: Handle,
        interconnect: Interconnect,
        config: Config,
    ) -> Self {
        let bitrate = DEFAULT_BITRATE;
        let encoder = new_encoder(bitrate, config.mix_mode)
            .expect("Failed to create encoder in mixing thread with known-good values.");
        let soft_clip = SoftClip::new(config.mix_mode.to_opus());

        let mut packet = [0u8; VOICE_PACKET_MAX];

        let mut rtp = MutableRtpPacket::new(&mut packet[..]).expect(
            "FATAL: Too few bytes in self.packet for RTP header.\
                (Blame: VOICE_PACKET_MAX?)",
        );
        rtp.set_version(RTP_VERSION);
        rtp.set_payload_type(RTP_PROFILE_TYPE);
        rtp.set_sequence(random::<u16>().into());
        rtp.set_timestamp(random::<u32>().into());

        let tracks = Vec::with_capacity(1.max(config.preallocated_tracks));
        let track_handles = Vec::with_capacity(1.max(config.preallocated_tracks));

        // Create an object disposal thread here.
        let (disposer, disposal_rx) = flume::unbounded();
        std::thread::spawn(move || disposal::runner(disposal_rx));

        let thread_pool = BlockyTaskPool::new(async_handle);

        let symph_layout = config.mix_mode.symph_layout();

        let config = config.into();

        let sample_buffer = SampleBuffer::<f32>::new(
            MONO_FRAME_SIZE as u64,
            symphonia_core::audio::SignalSpec::new_with_layout(
                SAMPLE_RATE_RAW as u32,
                symph_layout,
            ),
        );
        let symph_mix = AudioBuffer::<f32>::new(
            MONO_FRAME_SIZE as u64,
            symphonia_core::audio::SignalSpec::new_with_layout(
                SAMPLE_RATE_RAW as u32,
                symph_layout,
            ),
        );
        let resample_scratch = AudioBuffer::<f32>::new(
            MONO_FRAME_SIZE as u64,
            SignalSpec::new_with_layout(SAMPLE_RATE_RAW as u32, Layout::Stereo),
        );

        Self {
            bitrate,
            config,
            conn_active: None,
            content_prep_sequence: 0,
            deadline: Instant::now(),
            disposer,
            encoder,
            interconnect,
            mix_rx,
            muted: false,
            packet,
            prevent_events: false,
            silence_frames: 0,
            skip_sleep: false,
            soft_clip,
            thread_pool,
            ws: None,

            tracks,
            track_handles,

            sample_buffer,
            symph_mix,
            resample_scratch,

            #[cfg(test)]
            remaining_loops: None,
        }
    }

    fn run(&mut self) {
        let mut events_failure = false;
        let mut conn_failure = false;

        'runner: loop {
            if self.conn_active.is_some() || self.config.override_connection.is_some() {
                loop {
                    match self.mix_rx.try_recv() {
                        Ok(m) => {
                            let (events, conn, should_exit) = self.handle_message(m);
                            events_failure |= events;
                            conn_failure |= conn;

                            if should_exit {
                                break 'runner;
                            }
                        },

                        Err(TryRecvError::Disconnected) => {
                            break 'runner;
                        },

                        Err(TryRecvError::Empty) => {
                            break;
                        },
                    };
                }

                // The above action may have invalidated the connection; need to re-check!
                // Also, if we're in a test mode we should unconditionally run packet mixing code.
                if self.conn_active.is_some() || self.config.override_connection.is_some() {
                    if let Err(e) = self.cycle().and_then(|_| self.audio_commands_events()) {
                        events_failure |= e.should_trigger_interconnect_rebuild();
                        conn_failure |= e.should_trigger_connect();

                        debug!("Mixer thread cycle: {:?}", e);
                    }
                }
            } else {
                match self.mix_rx.recv() {
                    Ok(m) => {
                        let (events, conn, should_exit) = self.handle_message(m);
                        events_failure |= events;
                        conn_failure |= conn;

                        if should_exit {
                            break 'runner;
                        }
                    },
                    Err(_) => {
                        break 'runner;
                    },
                }
            }

            // event failure? rebuild interconnect.
            // ws or udp failure? full connect
            // (soft reconnect is covered by the ws task.)
            //
            // in both cases, send failure is fatal,
            // but will only occur on disconnect.
            // expecting this is fairly noisy, so exit silently.
            if events_failure {
                self.prevent_events = true;
                let sent = self
                    .interconnect
                    .core
                    .send(CoreMessage::RebuildInterconnect);
                events_failure = false;

                if sent.is_err() {
                    break;
                }
            }

            if conn_failure {
                self.conn_active = None;
                let sent = self.interconnect.core.send(CoreMessage::FullReconnect);
                conn_failure = false;

                if sent.is_err() {
                    break;
                }
            }
        }
    }

    #[inline]
    fn handle_message(&mut self, msg: MixerMessage) -> (bool, bool, bool) {
        let mut events_failure = false;
        let mut conn_failure = false;
        let mut should_exit = false;

        let error = match msg {
            MixerMessage::AddTrack(t) => self.add_track(t),
            MixerMessage::SetTrack(t) => {
                self.tracks.clear();

                let mut out = self.fire_event(EventMessage::RemoveAllTracks);

                if let Some(t) = t {
                    // Do this unconditionally: this affects local state infallibly,
                    // with the event installation being the remote part.
                    if let Err(e) = self.add_track(t) {
                        out = Err(e);
                    }
                }

                out
            },
            MixerMessage::SetBitrate(b) => {
                self.bitrate = b;
                if let Err(e) = self.set_bitrate(b) {
                    error!("Failed to update bitrate {:?}", e);
                }
                Ok(())
            },
            MixerMessage::SetMute(m) => {
                self.muted = m;
                Ok(())
            },
            MixerMessage::SetConn(conn, ssrc) => {
                self.conn_active = Some(conn);
                let mut rtp = MutableRtpPacket::new(&mut self.packet[..]).expect(
                    "Too few bytes in self.packet for RTP header.\
                        (Blame: VOICE_PACKET_MAX?)",
                );
                rtp.set_ssrc(ssrc);
                rtp.set_sequence(random::<u16>().into());
                rtp.set_timestamp(random::<u32>().into());
                self.deadline = Instant::now();
                Ok(())
            },
            MixerMessage::DropConn => {
                self.conn_active = None;
                Ok(())
            },
            MixerMessage::ReplaceInterconnect(i) => {
                self.prevent_events = false;
                if let Some(ws) = &self.ws {
                    conn_failure |= ws.send(WsMessage::ReplaceInterconnect(i.clone())).is_err();
                }
                if let Some(conn) = &self.conn_active {
                    conn_failure |= conn
                        .udp_rx
                        .send(UdpRxMessage::ReplaceInterconnect(i.clone()))
                        .is_err();
                }

                self.interconnect = i;

                self.rebuild_tracks()
            },
            MixerMessage::SetConfig(new_config) => {
                if new_config.mix_mode != self.config.mix_mode {
                    self.soft_clip = SoftClip::new(new_config.mix_mode.to_opus());
                    if let Ok(enc) = new_encoder(self.bitrate, new_config.mix_mode) {
                        self.encoder = enc;
                    } else {
                        self.bitrate = DEFAULT_BITRATE;
                        self.encoder = new_encoder(self.bitrate, new_config.mix_mode)
                            .expect("Failed fallback rebuild of OpusEncoder with safe inputs.");
                    }

                    let sl = new_config.mix_mode.symph_layout();
                    self.sample_buffer = SampleBuffer::<f32>::new(
                        MONO_FRAME_SIZE as u64,
                        SignalSpec::new_with_layout(SAMPLE_RATE_RAW as u32, sl),
                    );
                    self.symph_mix = AudioBuffer::<f32>::new(
                        MONO_FRAME_SIZE as u64,
                        SignalSpec::new_with_layout(SAMPLE_RATE_RAW as u32, sl),
                    );
                }

                self.config = Arc::new(new_config.clone());

                if self.tracks.capacity() < self.config.preallocated_tracks {
                    self.tracks
                        .reserve(self.config.preallocated_tracks - self.tracks.len());
                }

                if let Some(conn) = &self.conn_active {
                    conn_failure |= conn
                        .udp_rx
                        .send(UdpRxMessage::SetConfig(new_config))
                        .is_err();
                }

                Ok(())
            },
            MixerMessage::RebuildEncoder => match new_encoder(self.bitrate, self.config.mix_mode) {
                Ok(encoder) => {
                    self.encoder = encoder;
                    Ok(())
                },
                Err(e) => {
                    error!("Failed to rebuild encoder. Resetting bitrate. {:?}", e);
                    self.bitrate = DEFAULT_BITRATE;
                    self.encoder = new_encoder(self.bitrate, self.config.mix_mode)
                        .expect("Failed fallback rebuild of OpusEncoder with safe inputs.");
                    Ok(())
                },
            },
            MixerMessage::Ws(new_ws_handle) => {
                self.ws = new_ws_handle;
                Ok(())
            },
            MixerMessage::Poison => {
                should_exit = true;
                Ok(())
            },
        };

        if let Err(e) = error {
            events_failure |= e.should_trigger_interconnect_rebuild();
            conn_failure |= e.should_trigger_connect();
        }

        (events_failure, conn_failure, should_exit)
    }

    #[inline]
    fn fire_event(&self, event: EventMessage) -> Result<()> {
        // As this task is responsible for noticing the potential death of an event context,
        // it's responsible for not forcibly recreating said context repeatedly.
        if !self.prevent_events {
            self.interconnect.events.send(event)?;
        }

        Ok(())
    }

    #[inline]
    pub fn add_track(&mut self, track: TrackContext) -> Result<()> {
        let (track, evts, state, handle) = InternalTrack::decompose_track(track);
        self.tracks.push(track);
        self.track_handles.push(handle.clone());
        self.interconnect
            .events
            .send(EventMessage::AddTrack(evts, state, handle))?;

        Ok(())
    }

    // rebuilds the event thread's view of each track, in event of a full rebuild.
    #[inline]
    fn rebuild_tracks(&mut self) -> Result<()> {
        for (track, handle) in self.tracks.iter().zip(self.track_handles.iter()) {
            let evts = EventStore::default();
            let state = track.state();
            let handle = handle.clone();

            self.interconnect
                .events
                .send(EventMessage::AddTrack(evts, state, handle))?;
        }

        Ok(())
    }

    #[inline]
    fn audio_commands_events(&mut self) -> Result<()> {
        // Apply user commands.
        for (i, track) in self.tracks.iter_mut().enumerate() {
            // This causes fallible event system changes,
            // but if the event thread has died then we'll certainly
            // detect that on the tick later.
            // Changes to play state etc. MUST all be handled.
            let action = track.process_commands(i, &self.interconnect);

            if let Some(time) = action.seek_point {
                let full_input = &mut track.input;
                let time = Time::from(time.as_secs_f64());
                let mut ts = SeekTo::Time {
                    time,
                    track_id: None,
                };
                let (tx, rx) = flume::bounded(1);

                let queued_seek = if matches!(full_input, InputState::Preparing(_)) {
                    Some(util::copy_seek_to(&ts))
                } else {
                    None
                };

                let mut new_state = InputState::Preparing(PreparingInfo {
                    time: Instant::now(),
                    callback: rx,
                    queued_seek,
                });

                std::mem::swap(full_input, &mut new_state);

                match new_state {
                    InputState::Ready(p, r) => {
                        if let SeekTo::Time { time: _, track_id } = &mut ts {
                            *track_id = Some(p.track_id);
                        }

                        self.thread_pool
                            .seek(tx, p, r, ts, true, self.config.clone());
                    },
                    InputState::Preparing(old_prep) => {
                        // Annoying case: we need to mem_swap for the other two cases,
                        // but here we don't want to.
                        // new_state contains the old request now, so we want to move its
                        // callback and time *back* into self.full_inputs[i].
                        if let InputState::Preparing(new_prep) = full_input {
                            new_prep.callback = old_prep.callback;
                            new_prep.time = old_prep.time;
                        } else {
                            unreachable!()
                        }
                    },
                    InputState::NotReady(lazy) =>
                        self.thread_pool
                            .create(tx, lazy, Some(ts), self.config.clone()),
                }
            }

            if action.make_playable {
                if let Err(e) = track.get_or_ready_input(
                    i,
                    &self.interconnect,
                    &self.thread_pool,
                    &self.config,
                    self.prevent_events,
                ) {
                    if let Some(fail) = e.into_user() {
                        track.playing = PlayMode::Errored(fail);
                    }
                }
            }
        }

        // TODO: do without vec?
        let mut i = 0;
        let mut to_remove = Vec::with_capacity(self.tracks.len());
        while i < self.tracks.len() {
            let track = self
                .tracks
                .get_mut(i)
                .expect("Tried to remove an illegal track index.");

            if track.playing.is_done() {
                let p_state = track.playing.clone();
                let to_drop = self.tracks.swap_remove(i);
                drop(
                    self.disposer
                        .send(DisposalMessage::Track(Box::new(to_drop))),
                );
                let to_drop = self.track_handles.swap_remove(i);
                drop(self.disposer.send(DisposalMessage::Handle(to_drop)));

                to_remove.push(i);
                self.fire_event(EventMessage::ChangeState(
                    i,
                    TrackStateChange::Mode(p_state),
                ))?;
            } else {
                i += 1;
            }
        }

        // Tick
        self.fire_event(EventMessage::Tick)?;

        // Then do removals.
        for i in &to_remove[..] {
            self.fire_event(EventMessage::RemoveTrack(*i))?;
        }

        Ok(())
    }

    #[inline]
    fn march_deadline(&mut self) {
        if self.skip_sleep {
            return;
        }

        // Timed is the usual, default case.
        // The others exist for end-to-end testing.
        match &self.config.tick_style {
            TickStyle::Timed => {
                std::thread::sleep(self.deadline.saturating_duration_since(Instant::now()));
                self.deadline += TIMESTEP_LENGTH;
            },
            TickStyle::UntimedWithExecLimit(_rx) => {
                #[cfg(test)]
                {
                    if self.remaining_loops.is_none() {
                        if let Ok(new_val) = _rx.recv() {
                            self.remaining_loops = Some(new_val);
                        }
                    }

                    if let Some(cnt) = self.remaining_loops.as_mut() {
                        if *cnt == 0 {
                            self.remaining_loops = None;
                        } else {
                            *cnt = cnt.wrapping_sub(1);
                        }
                    }
                }
            },
        }
    }

    pub fn cycle(&mut self) -> Result<()> {
        let mut mix_buffer = [0f32; STEREO_FRAME_SIZE];

        self.symph_mix.clear();
        self.symph_mix.render_reserved(Some(MONO_FRAME_SIZE));
        self.resample_scratch.clear();

        // Walk over all the audio files, combining into one audio frame according
        // to volume, play state, etc.
        let mut mix_len = {
            let out = self.mix_tracks();

            self.sample_buffer.copy_interleaved_typed(&self.symph_mix);

            out
        };

        if self.muted {
            mix_len = MixType::MixedPcm(0);
        }

        if mix_len == MixType::MixedPcm(0) {
            if self.silence_frames > 0 {
                self.silence_frames -= 1;

                // Explicit "Silence" frame.
                let mut rtp = MutableRtpPacket::new(&mut self.packet[..]).expect(
                    "FATAL: Too few bytes in self.packet for RTP header.\
                        (Blame: VOICE_PACKET_MAX?)",
                );

                let payload = rtp.payload_mut();

                (&mut payload[TAG_SIZE..TAG_SIZE + SILENT_FRAME.len()])
                    .copy_from_slice(&SILENT_FRAME[..]);

                mix_len = MixType::Passthrough(SILENT_FRAME.len());
            } else {
                // Per official guidelines, send 5x silence BEFORE we stop speaking.
                if let Some(ws) = &self.ws {
                    // NOTE: this should prevent a catastrophic thread pileup.
                    // A full reconnect might cause an inner closed connection.
                    // It's safer to leave the central task to clean this up and
                    // pass the mixer a new channel.
                    drop(ws.send(WsMessage::Speaking(false)));
                }

                self.march_deadline();

                match &self.config.override_connection {
                    Some(OutputMode::Raw(tx)) =>
                        drop(tx.send(crate::driver::test_config::TickMessage::NoEl)),
                    Some(OutputMode::Rtp(tx)) =>
                        drop(tx.send(crate::driver::test_config::TickMessage::NoEl)),
                    None => {},
                }

                return Ok(());
            }
        } else {
            self.silence_frames = 5;

            if let MixType::MixedPcm(n) = mix_len {
                // to apply soft_clip, we need this to be in a normal f32 buffer.
                // unfortunately, SampleBuffer does not expose a `.samples_mut()`.
                // hence, an extra copy...
                let samples_to_copy = self.config.mix_mode.channels() * n;

                (&mut mix_buffer[..samples_to_copy])
                    .copy_from_slice(&self.sample_buffer.samples()[..samples_to_copy]);

                self.soft_clip.apply(
                    (&mut mix_buffer[..])
                        .try_into()
                        .expect("Mix buffer is known to have a valid sample count (softclip)."),
                )?;
            }
        }

        if let Some(ws) = &self.ws {
            ws.send(WsMessage::Speaking(true))?;
        }

        // Wait till the right time to send this packet:
        // usually a 20ms tick, in test modes this is either a finite number of runs or user input.
        self.march_deadline();
        if let Some(OutputMode::Raw(tx)) = &self.config.override_connection {
            let msg = match mix_len {
                MixType::Passthrough(len) if len == SILENT_FRAME.len() => OutputMessage::Silent,
                MixType::Passthrough(len) => {
                    let rtp = RtpPacket::new(&self.packet[..]).expect(
                        "FATAL: Too few bytes in self.packet for RTP header.\
                            (Blame: VOICE_PACKET_MAX?)",
                    );
                    let payload = rtp.payload();
                    let opus_frame = (&payload[TAG_SIZE..][..len]).to_vec();

                    OutputMessage::Passthrough(opus_frame)
                },
                MixType::MixedPcm(_) => OutputMessage::Mixed(
                    mix_buffer[..self.config.mix_mode.sample_count_in_frame()].to_vec(),
                ),
            };

            drop(tx.send(msg.into()));
        } else {
            self.prep_and_send_packet(&mix_buffer, mix_len)?;
        }

        if matches!(mix_len, MixType::MixedPcm(a) if a > 0) {
            for plane in self.symph_mix.planes_mut().planes() {
                plane.fill(0.0);
            }
        }

        Ok(())
    }

    fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<()> {
        self.encoder.set_bitrate(bitrate).map_err(Into::into)
    }

    #[inline]
    fn prep_and_send_packet(&mut self, buffer: &[f32; 1920], mix_len: MixType) -> Result<()> {
        let conn = self
            .conn_active
            .as_mut()
            .expect("Shouldn't be mixing packets without access to a cipher + UDP dest.");

        let index = {
            let mut rtp = MutableRtpPacket::new(&mut self.packet[..]).expect(
                "FATAL: Too few bytes in self.packet for RTP header.\
                    (Blame: VOICE_PACKET_MAX?)",
            );

            let payload = rtp.payload_mut();
            let crypto_mode = conn.crypto_state.kind();

            let payload_len = match mix_len {
                MixType::Passthrough(opus_len) => opus_len,
                MixType::MixedPcm(_samples) => {
                    let total_payload_space = payload.len() - crypto_mode.payload_suffix_len();
                    self.encoder.encode_float(
                        &buffer[..self.config.mix_mode.sample_count_in_frame()],
                        &mut payload[TAG_SIZE..total_payload_space],
                    )?
                },
            };

            let final_payload_size = conn
                .crypto_state
                .write_packet_nonce(&mut rtp, TAG_SIZE + payload_len);

            // Packet encryption ignored in test modes.
            if self.config.override_connection.is_none() {
                conn.crypto_state.kind().encrypt_in_place(
                    &mut rtp,
                    &conn.cipher,
                    final_payload_size,
                )?;
            }

            RtpPacket::minimum_packet_size() + final_payload_size
        };

        if let Some(OutputMode::Rtp(tx)) = &self.config.override_connection {
            // Test mode: send unencrypted (compressed) packets to local receiver.
            drop(tx.send(self.packet[..index].to_vec().into()));
        } else {
            // Normal operation: send encrypted payload to UDP Tx task.

            // TODO: This is dog slow, don't do this.
            // Can we replace this with a shared ring buffer + semaphore?
            // or the BBQueue crate?
            conn.udp_tx
                .send(UdpTxMessage::Packet(self.packet[..index].to_vec()))?;
        }

        let mut rtp = MutableRtpPacket::new(&mut self.packet[..]).expect(
            "FATAL: Too few bytes in self.packet for RTP header.\
                (Blame: VOICE_PACKET_MAX?)",
        );
        rtp.set_sequence(rtp.get_sequence() + 1);
        rtp.set_timestamp(rtp.get_timestamp() + MONO_FRAME_SIZE as u32);

        Ok(())
    }

    #[inline]
    fn mix_tracks(&mut self) -> MixType {
        // Get a slice of bytes to write in data for Opus packet passthrough.
        let mut rtp = MutableRtpPacket::new(&mut self.packet[..]).expect(
            "FATAL: Too few bytes in self.packet for RTP header.\
                (Blame: VOICE_PACKET_MAX?)",
        );
        let payload = rtp.payload_mut();
        let opus_frame = &mut payload[TAG_SIZE..];

        // Opus frame passthrough.
        // This requires that we have only one track, who has volume 1.0, and an
        // Opus codec type (verified internally).
        let do_passthrough = self.tracks.len() == 1 && {
            let track = &self.tracks[0];
            (track.volume - 1.0).abs() < f32::EPSILON
        };

        let mut len = 0;
        for (i, track) in self.tracks.iter_mut().enumerate() {
            let vol = track.volume;

            if !track.playing.is_playing() {
                continue;
            }

            let input = track.get_or_ready_input(
                i,
                &self.interconnect,
                &self.thread_pool,
                &self.config,
                self.prevent_events,
            );

            let (input, mix_state) = match input {
                Ok(i) => i,
                Err(InputReadyingError::Waiting) => continue,
                // TODO: allow for retry in given time.
                Err(e) => {
                    if let Some(fail) = e.into_user() {
                        track.playing = PlayMode::Errored(fail);
                    }
                    continue;
                },
            };

            let opus_slot = if do_passthrough {
                Some(&mut *opus_frame)
            } else {
                None
            };

            let (mix_type, status) = mix_logic::mix_symph_indiv(
                &mut self.symph_mix,
                &mut self.resample_scratch,
                input,
                mix_state,
                vol,
                opus_slot,
            );

            let return_here = if let MixType::MixedPcm(pcm_len) = mix_type {
                len = len.max(pcm_len);
                false
            } else {
                if mix_state.passthrough == Passthrough::Inactive {
                    input.decoder.reset();
                }
                mix_state.passthrough = Passthrough::Active;
                true
            };

            // FIXME: allow Ended to trigger a seek/loop/revisit in the same mix cycle?
            // Would this be possible with special-casing to mark some inputs as fast
            // to recreate? Probably not doable in the general case.
            match status {
                MixStatus::Live => track.step_frame(),
                MixStatus::Errored(e) =>
                    track.playing = PlayMode::Errored(PlayError::Decode(e.into())),
                MixStatus::Ended if track.do_loop() => {
                    let _ = self.track_handles[i].seek_time(Duration::default());
                    if !self.prevent_events {
                        // position update is sent out later, when the seek concludes.
                        drop(self.interconnect.events.send(EventMessage::ChangeState(
                            i,
                            TrackStateChange::Loops(track.loops, false),
                        )));
                    }
                },
                MixStatus::Ended => {
                    track.end();
                },
            }

            // This needs to happen here due to borrow checker shenanigans.
            if return_here {
                return mix_type;
            }
        }

        MixType::MixedPcm(len)
    }
}

/// The mixing thread is a synchronous context due to its compute-bound nature.
///
/// We pass in an async handle for the benefit of some Input classes (e.g., restartables)
/// who need to run their restart code elsewhere and return blank data until such time.
#[instrument(skip(interconnect, mix_rx, async_handle))]
pub(crate) fn runner(
    interconnect: Interconnect,
    mix_rx: Receiver<MixerMessage>,
    async_handle: Handle,
    config: Config,
) {
    let mut mixer = Mixer::new(mix_rx, async_handle, interconnect, config);

    mixer.run();

    drop(mixer.disposer.send(DisposalMessage::Poison));
}
