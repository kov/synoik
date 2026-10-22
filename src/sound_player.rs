// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Event sounds — mutter's `MetaSoundPlayer` (`src/core/meta-sound-player.c`), which plays a
//! sound-theme event through libcanberra. Ours resolves the theme with [`crate::sound_theme`],
//! decodes with symphonia and plays a PipeWire stream, all on a worker thread of its own: the
//! lookup is a few dozen `stat`s and the decode is real work, and neither belongs on the
//! compositor's frame path.
//!
//! The stream carries `media.role = Notification`, which is what pipewire-pulse maps canberra's
//! `event` role to (`module-protocol-pulse/remap.c`) — so it rides the same per-role volume
//! GNOME Settings' "System Sounds" slider sets.

/// Plays event sounds. Live sessions use [`PwSoundPlayer`]; tests install a recording stub.
pub trait SoundPlayer {
    /// `meta_sound_player_play_from_theme`: play `event_id` from the XDG sound `theme`.
    /// `description` names the stream (`CA_PROP_EVENT_DESCRIPTION`).
    fn play_from_theme(&self, theme: &str, event_id: &str, description: &str);
}

/// A [`SoundPlayer`] that records what it was asked to play.
///
/// Cloning shares one record, so a test can hand a clone to the compositor and keep its own
/// handle to assert on.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct StubSoundPlayer {
    played: std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>,
}

#[cfg(test)]
impl StubSoundPlayer {
    /// Every `(theme, event_id)` played so far.
    pub fn played(&self) -> Vec<(String, String)> {
        self.played.borrow().clone()
    }
}

#[cfg(test)]
impl SoundPlayer for StubSoundPlayer {
    fn play_from_theme(&self, theme: &str, event_id: &str, _description: &str) {
        self.played
            .borrow_mut()
            .push((theme.to_owned(), event_id.to_owned()));
    }
}

/// Decoded audio: interleaved `f32` frames.
#[cfg(feature = "audio")]
#[derive(Debug)]
pub struct Pcm {
    pub rate: u32,
    pub channels: u32,
    pub samples: Vec<f32>,
}

/// Decode a whole sound file: Ogg Vorbis (`.oga`/`.ogg`) or WAV, canberra's suffixes.
#[cfg(feature = "audio")]
pub fn decode(path: &std::path::Path) -> anyhow::Result<Pcm> {
    use anyhow::Context as _;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path)?;
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe().probe(
        &hint,
        source,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .context("no audio track")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .context("no audio codec parameters")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())?;

    let mut pcm = Pcm {
        rate: 0,
        channels: 0,
        samples: Vec::new(),
    };
    let mut packet_samples: Vec<f32> = Vec::new();
    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buffer) => {
                let spec = buffer.spec();
                pcm.rate = spec.rate();
                pcm.channels = spec.channels().count() as u32;
                buffer.copy_to_vec_interleaved(&mut packet_samples);
                pcm.samples.extend_from_slice(&packet_samples);
            }
            // A corrupt packet loses that packet, not the sound.
            Err(Error::DecodeError(_)) => (),
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::ensure!(pcm.rate > 0 && pcm.channels > 0, "no audio decoded");
    Ok(pcm)
}

#[cfg(feature = "audio")]
pub use pw::PwSoundPlayer;

#[cfg(feature = "audio")]
mod pw {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use pipewire::channel::Sender;
    use pipewire::context::ContextRc;
    use pipewire::core::{CoreRc, Listener as CoreListener, PW_ID_CORE};
    use pipewire::main_loop::MainLoopRc;
    use pipewire::properties::properties;
    use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
    use pipewire::{keys, spa};

    use super::{decode, Pcm, SoundPlayer};
    use crate::sound_theme::{self, Lookup};

    /// How many event sounds play at once. A terminal `cat`ting a binary is a bell storm, and
    /// one stream per bell would flood the graph; past this, a bell is silent.
    const MAX_STREAMS: usize = 4;

    /// How long past its own length a stream may linger before it is torn down regardless —
    /// a stream that never drains (no sink, a stalled graph) must not pin a node forever.
    const DRAIN_GRACE: Duration = Duration::from_secs(2);

    enum Msg {
        Play {
            theme: String,
            event_id: String,
            description: String,
        },
        /// A stream finished: tear it down from the loop, never from its own callback.
        Reap,
    }

    /// The compositor's handle: a channel to the worker thread, spawned on the first sound.
    #[derive(Default)]
    pub struct PwSoundPlayer {
        sender: OnceCell<Option<Sender<Msg>>>,
    }

    impl PwSoundPlayer {
        pub fn new() -> Self {
            Self::default()
        }

        fn sender(&self) -> Option<&Sender<Msg>> {
            self.sender
                .get_or_init(|| {
                    let (sender, receiver) = pipewire::channel::channel();
                    let reaper = sender.clone();
                    std::thread::Builder::new()
                        .name("synoik-sound".to_owned())
                        .spawn(move || {
                            if let Err(err) = run(receiver, reaper) {
                                warn!("event sounds disabled: {err:?}");
                            }
                        })
                        .map_err(|err| warn!("error spawning the sound thread: {err:?}"))
                        .ok()
                        .map(|_| sender)
                })
                .as_ref()
        }
    }

    impl SoundPlayer for PwSoundPlayer {
        fn play_from_theme(&self, theme: &str, event_id: &str, description: &str) {
            let Some(sender) = self.sender() else { return };
            // Fails only once the worker is gone, which it already logged.
            let _ = sender.send(Msg::Play {
                theme: theme.to_owned(),
                event_id: event_id.to_owned(),
                description: description.to_owned(),
            });
        }
    }

    fn run(receiver: pipewire::channel::Receiver<Msg>, reaper: Sender<Msg>) -> anyhow::Result<()> {
        pipewire::init();
        let main_loop = MainLoopRc::new(None)?;
        let context = ContextRc::new(&main_loop, None)?;
        let worker = RefCell::new(Worker {
            context,
            conn: None,
            playing: Vec::new(),
            cache: HashMap::new(),
            reaper,
        });
        let _receiver = receiver.attach(main_loop.loop_(), move |msg| {
            let mut worker = worker.borrow_mut();
            match msg {
                Msg::Play {
                    theme,
                    event_id,
                    description,
                } => worker.play(&theme, &event_id, &description),
                Msg::Reap => worker.reap(),
            }
        });
        main_loop.run();
        Ok(())
    }

    /// One connection to the daemon; replaced when the daemon goes away.
    struct Conn {
        core: CoreRc,
        _listener: CoreListener,
        lost: Rc<Cell<bool>>,
    }

    /// A decoded file, valid while the file's size and mtime hold — the alert-sound chooser
    /// swaps a symlink under an unchanged theme name.
    struct Cached {
        stamp: (SystemTime, u64),
        pcm: Arc<Pcm>,
    }

    struct Playing {
        // Before `stream`: the listener must be unhooked before the stream is destroyed.
        _listener: StreamListener<Progress>,
        stream: StreamRc,
        done: Rc<Cell<bool>>,
        deadline: Instant,
    }

    struct Progress {
        pcm: Arc<Pcm>,
        pos: usize,
        flushed: bool,
        done: Rc<Cell<bool>>,
        reaper: Sender<Msg>,
    }

    impl Progress {
        fn finish(&mut self) {
            if !self.done.replace(true) {
                let _ = self.reaper.send(Msg::Reap);
            }
        }
    }

    struct Worker {
        context: ContextRc,
        conn: Option<Conn>,
        playing: Vec<Playing>,
        cache: HashMap<PathBuf, Cached>,
        reaper: Sender<Msg>,
    }

    impl Worker {
        fn play(&mut self, theme: &str, event_id: &str, description: &str) {
            self.reap();
            if self.playing.len() >= MAX_STREAMS {
                trace!("dropping event sound {event_id}: {MAX_STREAMS} already playing");
                return;
            }
            let data_dirs = crate::gnome::xdg_data_dirs();
            let locale = sound_theme::messages_locale();
            let path = match sound_theme::lookup(&data_dirs, theme, event_id, &locale) {
                Lookup::Found(path) => path,
                Lookup::Disabled => return,
                Lookup::NotFound => {
                    debug!("no sound for {event_id} in theme {theme}");
                    return;
                }
            };
            let pcm = match self.decoded(path) {
                Ok(pcm) => pcm,
                Err(err) => {
                    warn!("error decoding the {event_id} sound: {err:?}");
                    return;
                }
            };
            if let Err(err) = self.start(pcm, event_id, description) {
                warn!("error playing the {event_id} sound: {err:?}");
            }
        }

        fn decoded(&mut self, path: PathBuf) -> anyhow::Result<Arc<Pcm>> {
            let meta = std::fs::metadata(&path)?;
            let stamp = (meta.modified()?, meta.len());
            if let Some(cached) = self.cache.get(&path).filter(|c| c.stamp == stamp) {
                return Ok(cached.pcm.clone());
            }
            let pcm = Arc::new(decode(&path)?);
            anyhow::ensure!(
                matches!(pcm.channels, 1 | 2),
                "{} channels; event sounds are mono or stereo",
                pcm.channels
            );
            self.cache.insert(
                path,
                Cached {
                    stamp,
                    pcm: pcm.clone(),
                },
            );
            Ok(pcm)
        }

        fn core(&mut self) -> anyhow::Result<CoreRc> {
            if self.conn.as_ref().is_some_and(|c| c.lost.get()) {
                // The streams went with the daemon.
                self.playing.clear();
                self.conn = None;
            }
            if let Some(conn) = &self.conn {
                return Ok(conn.core.clone());
            }
            let core = self.context.connect_rc(None)?;
            let lost = Rc::new(Cell::new(false));
            let listener = core
                .add_listener_local()
                .error({
                    let lost = lost.clone();
                    move |id, _seq, res, message| {
                        if id == PW_ID_CORE && res == -libc::EPIPE {
                            debug!("sound connection lost: {message}");
                            lost.set(true);
                        }
                    }
                })
                .register();
            self.conn = Some(Conn {
                core: core.clone(),
                _listener: listener,
                lost,
            });
            Ok(core)
        }

        fn start(
            &mut self,
            pcm: Arc<Pcm>,
            event_id: &str,
            description: &str,
        ) -> anyhow::Result<()> {
            let core = self.core()?;
            let stream = StreamRc::new(
                core,
                "synoik-event-sound",
                properties! {
                    *keys::MEDIA_TYPE => "Audio",
                    *keys::MEDIA_CATEGORY => "Playback",
                    *keys::MEDIA_ROLE => "Notification",
                    *keys::MEDIA_NAME => description,
                    *keys::APP_NAME => "synoik",
                    "event.id" => event_id,
                },
            )?;
            let done = Rc::new(Cell::new(false));
            let channels = pcm.channels as usize;
            let frames = pcm.samples.len() / channels;
            let length = Duration::from_secs_f64(frames as f64 / f64::from(pcm.rate));
            let progress = Progress {
                pcm: pcm.clone(),
                pos: 0,
                flushed: false,
                done: done.clone(),
                reaper: self.reaper.clone(),
            };
            let listener = stream
                .add_local_listener_with_user_data(progress)
                .process(fill)
                .drained(|_, progress| progress.finish())
                .state_changed(|_, progress, _, new| {
                    if let StreamState::Error(err) = new {
                        debug!("event sound stream failed: {err}");
                        progress.finish();
                    }
                })
                .register()?;

            let format = format_pod(&pcm)?;
            let mut params = [spa::pod::Pod::from_bytes(&format).expect("a serialized pod")];
            stream.connect(
                spa::utils::Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )?;
            self.playing.push(Playing {
                _listener: listener,
                stream,
                done,
                deadline: Instant::now() + length + DRAIN_GRACE,
            });
            // Wake the loop at the deadline, so a stream that never drains is reaped even when no
            // other sound follows it.
            let reaper = self.reaper.clone();
            std::thread::Builder::new()
                .name("synoik-sound-reap".to_owned())
                .spawn(move || {
                    std::thread::sleep(length + DRAIN_GRACE);
                    let _ = reaper.send(Msg::Reap);
                })?;
            Ok(())
        }

        fn reap(&mut self) {
            let now = Instant::now();
            self.playing.retain(|p| {
                let keep = !p.done.get() && now < p.deadline;
                if !keep {
                    let _ = p.stream.disconnect();
                }
                keep
            });
        }
    }

    /// The `process` callback: copy the next frames in, and drain once they have all gone out.
    fn fill(stream: &pipewire::stream::Stream, progress: &mut Progress) {
        if progress.flushed {
            return;
        }
        let channels = progress.pcm.channels as usize;
        let stride = channels * size_of::<f32>();
        {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let data = &mut buffer.datas_mut()[0];
            let mut n_frames = 0;
            if let Some(slice) = data.data() {
                let left = (progress.pcm.samples.len() - progress.pos) / channels;
                n_frames = (slice.len() / stride).min(left);
                let samples = &progress.pcm.samples[progress.pos..][..n_frames * channels];
                for (out, sample) in slice.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
                    *out = sample.to_le_bytes();
                }
                progress.pos += n_frames * channels;
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (n_frames * stride) as u32;
            // Dropping the buffer queues it, which has to happen before the flush.
        }
        if progress.pos >= progress.pcm.samples.len() {
            progress.flushed = true;
            if stream.flush(true).is_err() {
                progress.finish();
            }
        }
    }

    fn format_pod(pcm: &Pcm) -> anyhow::Result<Vec<u8>> {
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_rate(pcm.rate);
        info.set_channels(pcm.channels);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        if pcm.channels == 1 {
            position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
        } else {
            position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
            position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        }
        info.set_position(position);
        let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa::sys::SPA_TYPE_OBJECT_Format,
                id: spa::sys::SPA_PARAM_EnumFormat,
                properties: info.into(),
            }),
        )?;
        Ok(cursor.into_inner())
    }
}

#[cfg(all(test, feature = "audio"))]
mod tests {
    use super::*;

    /// A 16-bit stereo WAV decodes to interleaved `f32` at its own rate.
    #[test]
    fn a_wav_decodes_to_interleaved_floats() {
        let frames: [[i16; 2]; 3] = [[0, i16::MAX], [i16::MIN, 0], [16384, -16384]];
        let data: Vec<u8> = frames
            .iter()
            .flatten()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // channels
        wav.extend_from_slice(&22050u32.to_le_bytes());
        wav.extend_from_slice(&(22050u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);

        let path = std::env::temp_dir().join(format!("synoik-decode-{}.wav", std::process::id()));
        std::fs::write(&path, &wav).unwrap();
        let pcm = decode(&path);
        let _ = std::fs::remove_file(&path);
        let pcm = pcm.unwrap();

        assert_eq!((pcm.rate, pcm.channels), (22050, 2));
        assert_eq!(pcm.samples.len(), 6);
        assert_eq!(pcm.samples[0], 0.);
        assert!((pcm.samples[1] - 1.).abs() < 1e-3);
        assert_eq!(pcm.samples[2], -1.);
        assert_eq!(pcm.samples[4], 0.5);
    }
}
