//! Per-surface native audio service powered by `ohos-audio-binding` (`AudioRenderer`)
//! for low-latency polyphonic PCM short sounds and `ohos-avplayer-binding` (`AvPlayer`)
//! for compressed sound effects and streaming audio players.
//!
//! Owned directly by [`crate::render::mount::Mounter`], isolating sound/player IDs
//! per surface and guaranteeing deterministic teardown of native streams, players,
//! and staged temp files on `close()` / `Drop`.

use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_void},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::{fd::AsFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use nsh_core::{
    EventKind, EventPayload,
    audio::{AudioCommand, AudioError, AudioEvent, PlayerSettings},
};
use ohos_audio_binding::{
    AudioDataCallbackResult, AudioRenderer, AudioStreamBuilder, EncodingType, InterruptMode,
    LatencyMode, SampleFormat, StreamUsage,
};
use ohos_avplayer_binding::{
    self as av, AudioInterruptMode, AudioStreamUsage, AvPlayer, InfoField, InfoRef, SeekMode,
};
use rustc_hash::FxHashMap;

use crate::{
    app::{
        runtime::{Dispatcher, Message},
        session::Services,
    },
    services::{
        assets::StreamAssets,
        audio_spec::{
            MAX_VOICES, MIXER_SAMPLE_RATE, MixerState, clamp_av_rate, estimate_pcm_bytes, parse_wav,
        },
    },
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const RENDERER_IDLE_TIMEOUT: Duration = Duration::from_millis(500);

/// Owned temporary file in the Ability audio cache, removed on drop.
struct TempAudioFile {
    path: PathBuf,
}

impl TempAudioFile {
    fn from_bytes(
        cache_dir: &Path,
        prefix: &str,
        id: u32,
        hint: &str,
        bytes: &[u8],
    ) -> io::Result<Self> {
        let dir = cache_dir.join("audio");
        std::fs::create_dir_all(&dir)?;
        let ext = Path::new(hint.split(['?', '#']).next().unwrap_or(hint))
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("bin");
        let mut random = [0u8; 8];
        let _ = getrandom::fill(&mut random);
        let path = dir.join(format!(
            "{prefix}-{id}-{:016x}.{ext}",
            u64::from_le_bytes(random)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(bytes)?;
        Ok(Self { path })
    }

    fn from_reader(
        cache_dir: &Path,
        prefix: &str,
        id: u32,
        hint: &str,
        mut reader: impl Read,
    ) -> io::Result<Self> {
        let dir = cache_dir.join("audio");
        std::fs::create_dir_all(&dir)?;
        let ext = Path::new(hint.split(['?', '#']).next().unwrap_or(hint))
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("bin");
        let mut random = [0u8; 8];
        let _ = getrandom::fill(&mut random);
        let path = dir.join(format!(
            "{prefix}-{id}-{:016x}.{ext}",
            u64::from_le_bytes(random)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let mut buf = [0u8; 64 << 10];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
        }
        file.rewind()?;
        Ok(Self { path })
    }

    fn open_read(&self) -> io::Result<(File, i64)> {
        let file = File::open(&self.path)?;
        let len = i64::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::other("audio file exceeds i64 size"))?;
        if len <= 0 {
            return Err(io::Error::other("audio file is empty"));
        }
        Ok((file, len))
    }
}

impl Drop for TempAudioFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Clone, Copy)]
enum NoticeTarget {
    SoundProbe { sound_id: u32, generation: u64 },
    SoundVoice { voice_id: u64 },
    Player { player_id: u32, generation: u64 },
}

enum AvNoticeKind {
    State(i32),
    SeekDone(i32),
    Buffering(i32),
    Error(String),
}

enum WorkerEvent {
    Command(AudioCommand),
    AvNotice {
        target: NoticeTarget,
        kind: AvNoticeKind,
    },
    Shutdown,
}

struct AvCallbacks {
    tx: mpsc::Sender<WorkerEvent>,
    target: NoticeTarget,
}

unsafe extern "C" fn on_av_info(
    _: *mut av::RawPlayer,
    kind: av::InfoType,
    body: *mut av::RawFormat,
    userdata: *mut c_void,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(cb) = (unsafe { userdata.cast::<AvCallbacks>().as_ref() }) else {
            return;
        };
        let body = unsafe { InfoRef::from_raw(body) };
        let notice = match kind {
            av::INFO_STATE_CHANGE => body
                .as_ref()
                .and_then(|b| b.int(InfoField::State))
                .map(AvNoticeKind::State),
            av::INFO_SEEK_DONE => Some(AvNoticeKind::SeekDone(
                body.as_ref()
                    .and_then(|b| b.int(InfoField::SeekPosition))
                    .unwrap_or(-1),
            )),
            av::INFO_BUFFERING => body
                .as_ref()
                .and_then(|b| b.int(InfoField::BufferingType))
                .map(AvNoticeKind::Buffering),
            _ => None,
        };
        if let Some(kind) = notice {
            let _ = cb.tx.send(WorkerEvent::AvNotice {
                target: cb.target,
                kind,
            });
        }
    }));
}

unsafe extern "C" fn on_av_error(
    _: *mut av::RawPlayer,
    code: i32,
    message: *const std::ffi::c_char,
    userdata: *mut c_void,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(cb) = (unsafe { userdata.cast::<AvCallbacks>().as_ref() }) else {
            return;
        };
        let text = if message.is_null() {
            format!("AVPlayer error {code}")
        } else {
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        };
        let _ = cb.tx.send(WorkerEvent::AvNotice {
            target: cb.target,
            kind: AvNoticeKind::Error(text),
        });
    }));
}

struct OwnedAvPlayer {
    player: AvPlayer,
    _callbacks: Box<AvCallbacks>,
}

impl OwnedAvPlayer {
    fn new(tx: mpsc::Sender<WorkerEvent>, target: NoticeTarget) -> Result<Self, String> {
        let mut player = AvPlayer::new().map_err(|e| e.to_string())?;
        let mut callbacks = Box::new(AvCallbacks { tx, target });
        let ptr = (&mut *callbacks as *mut AvCallbacks).cast::<c_void>();
        // SAFETY: `OwnedAvPlayer::close` clears callbacks before dropping `callbacks`.
        unsafe {
            player
                .set_info_callback(Some(on_av_info), ptr)
                .map_err(|e| e.to_string())?;
            player
                .set_error_callback(Some(on_av_error), ptr)
                .map_err(|e| e.to_string())?;
        }
        let _ = player.set_audio_renderer_info(AudioStreamUsage::Music);
        let _ = player.set_audio_interrupt_mode(AudioInterruptMode::Share);
        Ok(Self {
            player,
            _callbacks: callbacks,
        })
    }

    fn close(mut self) {
        if let Err(error) = self.player.close_sync() {
            log::warn!("OHOS audio AVPlayer close failed: {error}");
            std::mem::forget(self);
        }
    }
}

struct CompressedSound {
    generation: u64,
    file: Arc<TempAudioFile>,
    max_duration: Duration,
    loaded: bool,
    truncate_after: Option<Duration>,
    probe: Option<OwnedAvPlayer>,
}

struct CompressedVoice {
    voice_id: u64,
    sound_id: u32,
    volume: f32,
    rate: f32,
    deadline: Option<Instant>,
    max_duration: Option<Duration>,
    _file: Arc<TempAudioFile>,
    av: OwnedAvPlayer,
}

struct ManagedPlayer {
    generation: u64,
    src: String,
    settings: PlayerSettings,
    temp_file: Option<TempAudioFile>,
    av: Option<OwnedAvPlayer>,
    prepared: bool,
    playing: bool,
    wants_play: bool,
    failed: bool,
    pending_seek_ms: Option<i32>,
    last_progress: Instant,
}

impl ManagedPlayer {
    fn is_http(&self) -> bool {
        self.src.starts_with("http://") || self.src.starts_with("https://")
    }

    fn duration(&mut self) -> Option<Duration> {
        if !self.prepared {
            return None;
        }
        self.av
            .as_mut()
            .and_then(|owned| owned.player.duration().ok())
            .filter(|&ms| ms > 0)
            .map(|ms| Duration::from_millis(ms as u64))
    }

    fn apply_settings(&mut self) {
        let Some(owned) = self.av.as_mut() else {
            return;
        };
        if !self.prepared {
            return;
        }
        let v = if self.settings.volume.is_finite() {
            self.settings.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let _ = owned.player.volume(v, v);
        let _ = owned.player.looping(self.settings.looped);
        let _ = owned
            .player
            .playback_rate(clamp_av_rate(self.settings.rate));
    }

    fn release_player(&mut self) {
        self.prepared = false;
        if let Some(av) = self.av.take() {
            av.close();
        }
    }
}

struct AudioWorker {
    host: Arc<Services>,
    route: Weak<Dispatcher>,
    assets: Arc<dyn StreamAssets>,
    tx: mpsc::Sender<WorkerEvent>,
    rx: mpsc::Receiver<WorkerEvent>,
    mixer: Arc<Mutex<MixerState>>,
    renderer: Option<AudioRenderer>,
    renderer_running: bool,
    renderer_idle_since: Option<Instant>,
    next_generation: u64,
    next_voice_id: u64,
    compressed_sounds: FxHashMap<u32, CompressedSound>,
    compressed_voices: Vec<CompressedVoice>,
    players: FxHashMap<u32, ManagedPlayer>,
}

impl AudioWorker {
    fn emit(&self, id: u32, event: AudioEvent) {
        if let Some(route) = self
            .route
            .upgrade()
            .filter(|d| d.live.load(Ordering::Acquire))
        {
            route.send(Message::HostEvent(
                EventKind::Audio,
                EventPayload::Audio { id, event },
            ));
        }
    }

    fn run(mut self) {
        loop {
            let timeout = self.next_timeout();
            match self.rx.recv_timeout(timeout) {
                Ok(WorkerEvent::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(WorkerEvent::Command(cmd)) => self.handle_command(cmd),
                Ok(WorkerEvent::AvNotice { target, kind }) => self.handle_av_notice(target, kind),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        self.shutdown();
    }

    fn next_timeout(&self) -> Duration {
        let mut wait = Duration::from_secs(60);
        if self.renderer_running {
            wait = wait.min(Duration::from_millis(100));
        }
        let now = Instant::now();
        for voice in &self.compressed_voices {
            if let Some(deadline) = voice.deadline {
                wait = wait.min(deadline.saturating_duration_since(now));
            }
        }
        for player in self.players.values() {
            if player.playing && player.prepared {
                let elapsed = now.saturating_duration_since(player.last_progress);
                wait = wait.min(PROGRESS_INTERVAL.saturating_sub(elapsed));
            }
        }
        wait
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // Auto-pause the OHAudio renderer after a short silent tail so we don't
        // hold hardware audio output open indefinitely between sound effects.
        if self.renderer_running {
            let active = self.mixer.lock().unwrap().has_active_voices();
            if active {
                self.renderer_idle_since = None;
            } else {
                let idle_since = *self.renderer_idle_since.get_or_insert(now);
                if now.saturating_duration_since(idle_since) >= RENDERER_IDLE_TIMEOUT {
                    if let Some(renderer) = self.renderer.as_mut() {
                        let _ = renderer.pause();
                    }
                    self.renderer_running = false;
                    self.renderer_idle_since = None;
                }
            }
        }
        // Stop any truncated compressed voices that reached their max_duration.
        let mut i = 0;
        while i < self.compressed_voices.len() {
            if self.compressed_voices[i]
                .deadline
                .is_some_and(|deadline| now >= deadline)
            {
                let voice = self.compressed_voices.remove(i);
                voice.av.close();
            } else {
                i += 1;
            }
        }
        // Emit periodic progress for playing streaming players.
        let ids: Vec<u32> = self
            .players
            .iter()
            .filter_map(|(&id, p)| {
                (p.playing
                    && p.prepared
                    && now.saturating_duration_since(p.last_progress) >= PROGRESS_INTERVAL)
                    .then_some(id)
            })
            .collect();
        for id in ids {
            self.emit_player_progress(id, now);
        }
    }

    fn ensure_renderer_started(&mut self) -> Result<(), String> {
        if self.renderer.is_none() {
            let mixer = self.mixer.clone();
            let mut builder = AudioStreamBuilder::new_renderer().map_err(|e| e.to_string())?;
            builder
                .sampling_rate(MIXER_SAMPLE_RATE as i32)
                .map_err(|e| e.to_string())?
                .channel_count(2)
                .map_err(|e| e.to_string())?
                .sample_format(SampleFormat::S16Le)
                .map_err(|e| e.to_string())?
                .encoding_type(EncodingType::Raw)
                .map_err(|e| e.to_string())?
                .latency_mode(LatencyMode::Fast)
                .map_err(|e| e.to_string())?
                .usage(StreamUsage::Music)
                .map_err(|e| e.to_string())?
                .interrupt_mode(InterruptMode::Share)
                .map_err(|e| e.to_string())?
                .write_data_callback(move |buf| {
                    if let Ok(mut guard) = mixer.lock() {
                        guard.mix_into_s16le(buf);
                    } else {
                        buf.fill(0);
                    }
                    AudioDataCallbackResult::Valid
                })
                .map_err(|e| e.to_string())?;
            let renderer = builder.build().map_err(|e| e.to_string())?;
            self.renderer = Some(renderer);
        }
        if !self.renderer_running
            && let Some(renderer) = self.renderer.as_mut()
        {
            renderer.start().map_err(|e| e.to_string())?;
            self.renderer_running = true;
            self.renderer_idle_since = None;
        }
        Ok(())
    }

    fn handle_command(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::Load {
                id,
                bytes,
                hint,
                max_duration,
            } => {
                self.unload_sound(id);
                if let Some(pcm) = parse_wav(&bytes, max_duration) {
                    let duration = pcm.duration;
                    let mem_bytes = pcm.bytes;
                    let truncated = pcm.truncated;
                    self.mixer.lock().unwrap().insert_sound(id, pcm);
                    self.emit(
                        id,
                        AudioEvent::Loaded {
                            duration,
                            bytes: mem_bytes,
                            truncated,
                        },
                    );
                    return;
                }
                self.load_compressed_sound(id, &bytes, &hint, max_duration);
            }
            AudioCommand::LoadUrl { id, .. } => {
                self.emit(
                    id,
                    AudioEvent::Failed {
                        error: AudioError::NotSupported,
                        message: "LoadUrl is not supported on native platforms".into(),
                    },
                );
            }
            AudioCommand::Play { id, volume, rate } => {
                let played_pcm = self.mixer.lock().unwrap().play(id, volume, rate);
                if played_pcm {
                    if let Err(error) = self.ensure_renderer_started() {
                        log::warn!("OHOS audio renderer start failed: {error}");
                    }
                    return;
                }
                self.play_compressed_sound(id, volume, rate);
            }
            AudioCommand::Stop { id } => {
                self.mixer.lock().unwrap().stop_sound(id);
                let mut i = 0;
                while i < self.compressed_voices.len() {
                    if self.compressed_voices[i].sound_id == id {
                        let voice = self.compressed_voices.remove(i);
                        voice.av.close();
                    } else {
                        i += 1;
                    }
                }
            }
            AudioCommand::Unload { id } => {
                self.unload_sound(id);
            }
            AudioCommand::Open { id, src, settings } => {
                self.close_player(id);
                self.next_generation += 1;
                self.players.insert(
                    id,
                    ManagedPlayer {
                        generation: self.next_generation,
                        src,
                        settings,
                        temp_file: None,
                        av: None,
                        prepared: false,
                        playing: false,
                        wants_play: false,
                        failed: false,
                        pending_seek_ms: None,
                        last_progress: Instant::now(),
                    },
                );
            }
            AudioCommand::Preload { id } => {
                self.ensure_player(id);
            }
            AudioCommand::Resume { id } => {
                let needs_ensure = if let Some(player) = self.players.get_mut(&id) {
                    if player.failed {
                        player.failed = false;
                        player.release_player();
                    }
                    if player.playing {
                        return;
                    }
                    player.wants_play = true;
                    if player.prepared
                        && let Some(av) = player.av.as_mut()
                    {
                        if let Err(err) = av.player.play() {
                            self.fail_player(id, err.to_string());
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    false
                };
                if needs_ensure {
                    self.ensure_player(id);
                }
            }
            AudioCommand::Pause { id } => {
                if let Some(player) = self.players.get_mut(&id) {
                    let was = player.playing || player.wants_play;
                    player.wants_play = false;
                    if player.playing
                        && let Some(av) = player.av.as_mut()
                    {
                        player.playing = false;
                        let _ = av.player.pause();
                        self.emit(id, AudioEvent::Paused);
                    } else if was {
                        player.playing = false;
                        self.emit(id, AudioEvent::Paused);
                    }
                }
            }
            AudioCommand::Seek { id, position } => {
                let target_ms = (position.as_secs_f64() * 1000.0)
                    .clamp(0.0, f64::from(i32::MAX))
                    .round() as i32;
                let Some(player) = self.players.get_mut(&id) else {
                    return;
                };
                if !player.prepared {
                    player.pending_seek_ms = Some(target_ms);
                    return;
                }
                let clamped = match player.duration() {
                    Some(dur) => {
                        target_ms.min((dur.as_millis() as i64).min(i64::from(i32::MAX)) as i32)
                    }
                    None => target_ms,
                };
                if let Some(av) = player.av.as_mut() {
                    let _ = av.player.seek(clamped, SeekMode::Closest);
                }
            }
            AudioCommand::Set { id, settings } => {
                if let Some(player) = self.players.get_mut(&id) {
                    player.settings = settings;
                    player.apply_settings();
                }
            }
            AudioCommand::Close { id } => {
                self.close_player(id);
            }
            _ => {}
        }
    }

    fn unload_sound(&mut self, id: u32) {
        self.mixer.lock().unwrap().unload_sound(id);
        let mut i = 0;
        while i < self.compressed_voices.len() {
            if self.compressed_voices[i].sound_id == id {
                let voice = self.compressed_voices.remove(i);
                voice.av.close();
            } else {
                i += 1;
            }
        }
        if let Some(mut entry) = self.compressed_sounds.remove(&id)
            && let Some(probe) = entry.probe.take()
        {
            probe.close();
        }
    }

    fn load_compressed_sound(&mut self, id: u32, bytes: &[u8], hint: &str, max_duration: Duration) {
        let file =
            match TempAudioFile::from_bytes(&self.host.cache_directory(), "sound", id, hint, bytes)
            {
                Ok(file) => Arc::new(file),
                Err(error) => {
                    self.emit(
                        id,
                        AudioEvent::Failed {
                            error: AudioError::Failed,
                            message: error.to_string(),
                        },
                    );
                    return;
                }
            };
        let (opened, len) = match file.open_read() {
            Ok(pair) => pair,
            Err(error) => {
                self.emit(
                    id,
                    AudioEvent::Failed {
                        error: AudioError::Decode,
                        message: error.to_string(),
                    },
                );
                return;
            }
        };
        self.next_generation += 1;
        let generation = self.next_generation;
        let mut probe = match OwnedAvPlayer::new(
            self.tx.clone(),
            NoticeTarget::SoundProbe {
                sound_id: id,
                generation,
            },
        ) {
            Ok(p) => p,
            Err(message) => {
                self.emit(
                    id,
                    AudioEvent::Failed {
                        error: AudioError::Decode,
                        message,
                    },
                );
                return;
            }
        };
        if let Err(err) = probe.player.set_fd(opened.as_fd(), 0, len) {
            probe.close();
            self.emit(
                id,
                AudioEvent::Failed {
                    error: AudioError::Decode,
                    message: err.to_string(),
                },
            );
            return;
        }
        self.compressed_sounds.insert(
            id,
            CompressedSound {
                generation,
                file,
                max_duration,
                loaded: false,
                truncate_after: None,
                probe: Some(probe),
            },
        );
    }

    fn play_compressed_sound(&mut self, id: u32, volume: f32, rate: f32) {
        let Some(entry) = self.compressed_sounds.get(&id).filter(|e| e.loaded) else {
            return;
        };
        let file = entry.file.clone();
        let max_duration = entry.truncate_after;
        let Ok((opened, len)) = file.open_read() else {
            return;
        };
        if self.compressed_voices.len() >= MAX_VOICES {
            let oldest = self.compressed_voices.remove(0);
            oldest.av.close();
        }
        self.next_voice_id += 1;
        let voice_id = self.next_voice_id;
        let Ok(mut av) = OwnedAvPlayer::new(self.tx.clone(), NoticeTarget::SoundVoice { voice_id })
        else {
            return;
        };
        if av.player.set_fd(opened.as_fd(), 0, len).is_err() {
            av.close();
            return;
        }
        self.compressed_voices.push(CompressedVoice {
            voice_id,
            sound_id: id,
            volume: if volume.is_finite() {
                volume.clamp(0.0, 1.0)
            } else {
                1.0
            },
            rate: clamp_av_rate(rate),
            deadline: None,
            max_duration,
            _file: file,
            av,
        });
    }

    fn ensure_player(&mut self, id: u32) {
        let (src, generation) = {
            let Some(player) = self.players.get(&id) else {
                return;
            };
            if player.av.is_some() || player.failed {
                return;
            }
            (player.src.clone(), player.generation)
        };
        let mut owned = match OwnedAvPlayer::new(
            self.tx.clone(),
            NoticeTarget::Player {
                player_id: id,
                generation,
            },
        ) {
            Ok(av) => av,
            Err(message) => {
                self.fail_player(id, message);
                return;
            }
        };
        let mut staged_temp: Option<TempAudioFile> = None;
        let source_result: Result<(), String> =
            if src.starts_with("http://") || src.starts_with("https://") {
                CString::new(src.as_str())
                    .map_err(|_| "invalid audio URL".into())
                    .and_then(|url| owned.player.set_url(&url).map_err(|e| e.to_string()))
            } else if let Some(rest) = src.strip_prefix("file://") {
                let decoded = percent_encoding::percent_decode_str(rest)
                    .decode_utf8_lossy()
                    .into_owned();
                set_file_path_source(&mut owned.player, Path::new(&decoded))
            } else if src.starts_with('/') {
                set_file_path_source(&mut owned.player, Path::new(&src))
            } else if src.starts_with("asset:/") {
                if let Some(file_result) = self.assets.open_file(&src) {
                    file_result.and_then(|file| set_opened_file_source(&mut owned.player, &file))
                } else {
                    self.assets
                        .reader(&src)
                        .and_then(|reader| {
                            TempAudioFile::from_reader(
                                &self.host.cache_directory(),
                                "player",
                                id,
                                &src,
                                reader,
                            )
                            .map_err(|e| e.to_string())
                        })
                        .and_then(|temp| {
                            let (file, len) = temp.open_read().map_err(|e| e.to_string())?;
                            owned
                                .player
                                .set_fd(file.as_fd(), 0, len)
                                .map_err(|e| e.to_string())?;
                            staged_temp = Some(temp);
                            Ok(())
                        })
                }
            } else {
                Err(format!("unsupported audio source: {src}"))
            };

        if let Err(message) = source_result {
            owned.close();
            self.fail_player(id, message);
            return;
        }
        if let Some(player) = self.players.get_mut(&id) {
            player.temp_file = staged_temp;
            player.av = Some(owned);
        } else {
            owned.close();
        }
    }

    fn fail_player(&mut self, id: u32, message: String) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if player.failed {
            return;
        }
        let error = if player.is_http() {
            AudioError::Network
        } else {
            AudioError::Decode
        };
        player.failed = true;
        player.wants_play = false;
        player.playing = false;
        player.release_player();
        self.emit(id, AudioEvent::Failed { error, message });
    }

    fn close_player(&mut self, id: u32) {
        if let Some(mut player) = self.players.remove(&id) {
            player.wants_play = false;
            player.playing = false;
            player.release_player();
        }
    }

    fn emit_player_progress(&mut self, id: u32, now: Instant) {
        let Some(player) = self.players.get_mut(&id) else {
            return;
        };
        if !player.prepared {
            return;
        }
        player.last_progress = now;
        let pos_ms = player
            .av
            .as_mut()
            .and_then(|av| av.player.current_time().ok())
            .unwrap_or(0)
            .max(0);
        let duration = player.duration();
        self.emit(
            id,
            AudioEvent::Progress {
                position: Duration::from_millis(pos_ms as u64),
                duration,
            },
        );
    }

    fn handle_av_notice(&mut self, target: NoticeTarget, kind: AvNoticeKind) {
        match target {
            NoticeTarget::SoundProbe {
                sound_id,
                generation,
            } => {
                let Some(entry) = self
                    .compressed_sounds
                    .get_mut(&sound_id)
                    .filter(|e| e.generation == generation)
                else {
                    return;
                };
                match kind {
                    AvNoticeKind::State(state) if state == av::STATE_INITIALIZED as i32 => {
                        if let Some(probe) = entry.probe.as_mut()
                            && let Err(err) = probe.player.prepare()
                        {
                            self.unload_sound(sound_id);
                            self.emit(
                                sound_id,
                                AudioEvent::Failed {
                                    error: AudioError::Decode,
                                    message: err.to_string(),
                                },
                            );
                        }
                    }
                    AvNoticeKind::State(state) if state == av::STATE_PREPARED as i32 => {
                        let duration_ms = entry
                            .probe
                            .as_mut()
                            .and_then(|p| p.player.duration().ok())
                            .unwrap_or(0)
                            .max(0) as u64;
                        if let Some(probe) = entry.probe.take() {
                            probe.close();
                        }
                        let raw_dur = Duration::from_millis(duration_ms);
                        let max_dur = entry.max_duration;
                        let truncated = !max_dur.is_zero() && raw_dur > max_dur;
                        let kept = if truncated { max_dur } else { raw_dur };
                        entry.loaded = true;
                        entry.truncate_after = truncated.then_some(kept);
                        let bytes = estimate_pcm_bytes(kept, 44_100);
                        self.emit(
                            sound_id,
                            AudioEvent::Loaded {
                                duration: kept,
                                bytes,
                                truncated,
                            },
                        );
                    }
                    AvNoticeKind::Error(message) => {
                        self.unload_sound(sound_id);
                        self.emit(
                            sound_id,
                            AudioEvent::Failed {
                                error: AudioError::Decode,
                                message,
                            },
                        );
                    }
                    _ => {}
                }
            }
            NoticeTarget::SoundVoice { voice_id } => {
                let Some(idx) = self
                    .compressed_voices
                    .iter()
                    .position(|v| v.voice_id == voice_id)
                else {
                    return;
                };
                match kind {
                    AvNoticeKind::State(state) if state == av::STATE_INITIALIZED as i32 => {
                        if self.compressed_voices[idx].av.player.prepare().is_err() {
                            let voice = self.compressed_voices.remove(idx);
                            voice.av.close();
                        }
                    }
                    AvNoticeKind::State(state) if state == av::STATE_PREPARED as i32 => {
                        let voice = &mut self.compressed_voices[idx];
                        let _ = voice.av.player.volume(voice.volume, voice.volume);
                        let _ = voice.av.player.playback_rate(voice.rate);
                        if voice.av.player.play().is_err() {
                            let voice = self.compressed_voices.remove(idx);
                            voice.av.close();
                        } else if let Some(max_dur) = voice.max_duration {
                            let wall = Duration::from_secs_f64(
                                max_dur.as_secs_f64() / f64::from(voice.rate.max(0.25)),
                            );
                            voice.deadline = Some(Instant::now() + wall);
                        }
                    }
                    AvNoticeKind::State(state)
                        if state == av::STATE_COMPLETED as i32
                            || state == av::STATE_STOPPED as i32
                            || state == av::STATE_RELEASED as i32 =>
                    {
                        let voice = self.compressed_voices.remove(idx);
                        voice.av.close();
                    }
                    AvNoticeKind::Error(_) => {
                        let voice = self.compressed_voices.remove(idx);
                        voice.av.close();
                    }
                    _ => {}
                }
            }
            NoticeTarget::Player {
                player_id,
                generation,
            } => {
                let valid = self
                    .players
                    .get(&player_id)
                    .is_some_and(|p| p.generation == generation && p.av.is_some());
                if !valid {
                    return;
                }
                match kind {
                    AvNoticeKind::State(state) if state == av::STATE_INITIALIZED as i32 => {
                        if let Some(player) = self.players.get_mut(&player_id)
                            && let Some(av) = player.av.as_mut()
                            && let Err(err) = av.player.prepare()
                        {
                            self.fail_player(player_id, err.to_string());
                        }
                    }
                    AvNoticeKind::State(state) if state == av::STATE_PREPARED as i32 => {
                        let (duration, pending_seek, wants_play) = {
                            let player = self.players.get_mut(&player_id).unwrap();
                            player.prepared = true;
                            player.apply_settings();
                            (
                                player.duration(),
                                player.pending_seek_ms.take(),
                                player.wants_play,
                            )
                        };
                        self.emit(player_id, AudioEvent::Load { duration });
                        if let Some(target_ms) = pending_seek
                            && let Some(player) = self.players.get_mut(&player_id)
                        {
                            let clamped = match duration {
                                Some(d) => target_ms
                                    .min((d.as_millis() as i64).min(i64::from(i32::MAX)) as i32),
                                None => target_ms,
                            };
                            if let Some(av) = player.av.as_mut() {
                                let _ = av.player.seek(clamped, SeekMode::Closest);
                            }
                        }
                        if wants_play
                            && let Some(player) = self.players.get_mut(&player_id)
                            && let Some(av) = player.av.as_mut()
                            && let Err(err) = av.player.play()
                        {
                            self.fail_player(player_id, err.to_string());
                        }
                    }
                    AvNoticeKind::State(state) if state == av::STATE_PLAYING as i32 => {
                        if let Some(player) = self.players.get_mut(&player_id) {
                            player.playing = true;
                            player.last_progress = Instant::now();
                            player.apply_settings();
                        }
                        self.emit(player_id, AudioEvent::Playing);
                    }
                    AvNoticeKind::State(state) if state == av::STATE_PAUSED as i32 => {
                        let emit_paused = if let Some(player) = self.players.get_mut(&player_id) {
                            let was = player.playing;
                            player.playing = false;
                            was
                        } else {
                            false
                        };
                        if emit_paused {
                            self.emit(player_id, AudioEvent::Paused);
                        }
                    }
                    AvNoticeKind::State(state) if state == av::STATE_COMPLETED as i32 => {
                        let (looped, duration) = {
                            let player = self.players.get_mut(&player_id).unwrap();
                            if player.settings.looped {
                                return;
                            }
                            player.playing = false;
                            player.wants_play = false;
                            (false, player.duration())
                        };
                        if !looped {
                            if let Some(d) = duration {
                                self.emit(
                                    player_id,
                                    AudioEvent::Progress {
                                        position: d,
                                        duration: Some(d),
                                    },
                                );
                            }
                            self.emit(player_id, AudioEvent::Ended);
                        }
                    }
                    AvNoticeKind::SeekDone(pos_ms) => {
                        let (pos, duration) = {
                            let player = self.players.get_mut(&player_id).unwrap();
                            if !player.prepared {
                                return;
                            }
                            let ms = if pos_ms >= 0 {
                                pos_ms
                            } else {
                                player
                                    .av
                                    .as_mut()
                                    .and_then(|av| av.player.current_time().ok())
                                    .unwrap_or(0)
                                    .max(0)
                            };
                            (Duration::from_millis(ms as u64), player.duration())
                        };
                        self.emit(
                            player_id,
                            AudioEvent::Progress {
                                position: pos,
                                duration,
                            },
                        );
                    }
                    AvNoticeKind::Buffering(kind)
                        if kind == av::BUFFERING_START as i32
                            || kind == av::BUFFERING_END as i32 =>
                    {
                        self.emit(
                            player_id,
                            AudioEvent::Buffer {
                                buffering: kind == av::BUFFERING_START as i32,
                            },
                        );
                    }
                    AvNoticeKind::Buffering(_) => {}
                    AvNoticeKind::Error(message) => {
                        self.fail_player(player_id, message);
                    }
                    _ => {}
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.mixer.lock().unwrap().clear();
        if let Some(mut renderer) = self.renderer.take() {
            let _ = renderer.stop();
            let _ = renderer.close();
        }
        self.renderer_running = false;
        for voice in self.compressed_voices.drain(..) {
            voice.av.close();
        }
        for (_, mut entry) in self.compressed_sounds.drain() {
            if let Some(probe) = entry.probe.take() {
                probe.close();
            }
        }
        for (_, mut player) in self.players.drain() {
            player.release_player();
        }
    }
}

fn set_file_path_source(player: &mut AvPlayer, path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    set_opened_file_source(player, &file)
}

fn set_opened_file_source(player: &mut AvPlayer, file: &File) -> Result<(), String> {
    let len = i64::try_from(file.metadata().map_err(|e| e.to_string())?.len())
        .map_err(|_| "audio file exceeds i64 size".to_owned())?;
    if len <= 0 {
        return Err("audio file is empty".into());
    }
    player
        .set_fd(file.as_fd(), 0, len)
        .map_err(|e| e.to_string())
}

struct RunningAudio {
    tx: mpsc::Sender<WorkerEvent>,
    mixer: Arc<Mutex<MixerState>>,
    closed: AtomicBool,
}

/// Per-surface audio controller. Lazily spawns one worker thread on first audio
/// command and cleanly shuts down all streams, players, and temp files on close.
pub(crate) struct Audio {
    host: Arc<Services>,
    route: Weak<Dispatcher>,
    assets: Arc<dyn StreamAssets>,
    running: RefCell<Option<RunningAudio>>,
}

impl Audio {
    pub(crate) fn new(
        host: Arc<Services>,
        route: Weak<Dispatcher>,
        assets: Arc<dyn StreamAssets>,
    ) -> Self {
        Self {
            host,
            route,
            assets,
            running: RefCell::new(None),
        }
    }

    fn ensure_started(&self) -> napi_ohos::Result<()> {
        if self.running.borrow().is_some() {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel::<WorkerEvent>();
        let mixer = Arc::new(Mutex::new(MixerState::default()));
        let worker = AudioWorker {
            host: self.host.clone(),
            route: self.route.clone(),
            assets: self.assets.clone(),
            tx: tx.clone(),
            rx,
            mixer: mixer.clone(),
            renderer: None,
            renderer_running: false,
            renderer_idle_since: None,
            next_generation: 0,
            next_voice_id: 0,
            compressed_sounds: FxHashMap::default(),
            compressed_voices: Vec::new(),
            players: FxHashMap::default(),
        };
        std::thread::Builder::new()
            .name("nsh-audio".into())
            .spawn(move || worker.run())
            .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        *self.running.borrow_mut() = Some(RunningAudio {
            tx,
            mixer,
            closed: AtomicBool::new(false),
        });
        Ok(())
    }

    pub(crate) fn command(&self, command: AudioCommand) -> napi_ohos::Result<()> {
        if let AudioCommand::LoadUrl { id, .. } = command {
            if let Some(route) = self
                .route
                .upgrade()
                .filter(|d| d.live.load(Ordering::Acquire))
            {
                route.send(Message::HostEvent(
                    EventKind::Audio,
                    EventPayload::Audio {
                        id,
                        event: AudioEvent::Failed {
                            error: AudioError::NotSupported,
                            message: "LoadUrl is not supported on native platforms".into(),
                        },
                    },
                ));
            }
            return Ok(());
        }
        self.ensure_started()?;
        if let Some(running) = self.running.borrow().as_ref()
            && !running.closed.load(Ordering::Acquire)
        {
            let _ = running.tx.send(WorkerEvent::Command(command));
        }
        Ok(())
    }

    pub(crate) fn close(&self) {
        if let Some(running) = self.running.borrow_mut().take() {
            running.closed.store(true, Ordering::Release);
            // Silence PCM mixer immediately on close before worker thread finishes teardown.
            if let Ok(mut mixer) = running.mixer.lock() {
                mixer.clear();
            }
            let _ = running.tx.send(WorkerEvent::Shutdown);
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.close();
    }
}
