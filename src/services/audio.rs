//! Per-surface native audio service powered by `ohos-avplayer-binding` (`AvPlayer`)
//! for short `Sound` effects (polyphonic, up to 8 voices) and streaming `AudioPlayer`s.

use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_void},
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{fd::AsFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Weak, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use nsh_core::{
    EventKind, EventPayload,
    audio::{AudioCommand, AudioError, AudioEvent, PlayerSettings},
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
    services::assets::StreamAssets,
};

const MAX_VOICES: usize = 8;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() { v.clamp(lo, hi) } else { 1.0 }
}

fn emit_to(route: &Weak<Dispatcher>, id: u32, event: AudioEvent) {
    if let Some(d) = route.upgrade().filter(|d| d.live.load(Ordering::Acquire)) {
        d.send(Message::HostEvent(
            EventKind::Audio,
            EventPayload::Audio { id, event },
        ));
    }
}

fn set_opened_file(player: &mut AvPlayer, file: &File) -> Result<(), String> {
    let len = i64::try_from(file.metadata().map_err(|e| e.to_string())?.len())
        .map_err(|_| "audio file exceeds i64 size".to_owned())?;
    if len <= 0 {
        return Err("audio file is empty".into());
    }
    player
        .set_fd(file.as_fd(), 0, len)
        .map_err(|e| e.to_string())
}

/// Owned temporary file in the Ability audio cache, removed on drop.
struct TempAudioFile(PathBuf);

impl TempAudioFile {
    fn create(
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
        let temp = Self(path);
        io::copy(&mut reader, &mut file)?;
        Ok(temp)
    }
}

impl Drop for TempAudioFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Target {
    Probe(u32, u64),
    Voice(u64),
    Player(u32, u64),
}

enum AvNotice {
    State(i32),
    SeekDone(i32),
    Buffering(i32),
    Error(String),
}

enum WorkerEvent {
    Command(AudioCommand),
    Notice(Target, AvNotice),
    Shutdown,
}

struct AvCallbacks {
    tx: mpsc::Sender<WorkerEvent>,
    target: Target,
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
                .map(AvNotice::State),
            av::INFO_SEEK_DONE => Some(AvNotice::SeekDone(
                body.as_ref()
                    .and_then(|b| b.int(InfoField::SeekPosition))
                    .unwrap_or(-1),
            )),
            av::INFO_BUFFERING => body
                .as_ref()
                .and_then(|b| b.int(InfoField::BufferingType))
                .map(AvNotice::Buffering),
            _ => None,
        };
        if let Some(n) = notice {
            let _ = cb.tx.send(WorkerEvent::Notice(cb.target, n));
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
        let _ = cb
            .tx
            .send(WorkerEvent::Notice(cb.target, AvNotice::Error(text)));
    }));
}

struct OwnedAvPlayer {
    player: std::mem::ManuallyDrop<AvPlayer>,
    callbacks: std::mem::ManuallyDrop<Box<AvCallbacks>>,
    closed: bool,
}

impl OwnedAvPlayer {
    fn new(tx: mpsc::Sender<WorkerEvent>, target: Target) -> Result<Self, String> {
        let callbacks = Box::new(AvCallbacks { tx, target });
        let player = AvPlayer::new().map_err(|e| e.to_string())?;
        let mut owned = Self {
            player: std::mem::ManuallyDrop::new(player),
            callbacks: std::mem::ManuallyDrop::new(callbacks),
            closed: false,
        };
        // The boxed allocation has a stable heap address while owned by `Self`;
        // `Drop` / `close_inner` tears down the native player via `close_sync`
        // before freeing `callbacks`, and leaks both if `close_sync` fails.
        let ptr = (&mut **owned.callbacks as *mut AvCallbacks).cast::<c_void>();
        // SAFETY: `ptr` points to `owned.callbacks` which outlives any native callbacks.
        unsafe {
            owned
                .player
                .set_info_callback(Some(on_av_info), ptr)
                .map_err(|e| e.to_string())?;
            owned
                .player
                .set_error_callback(Some(on_av_error), ptr)
                .map_err(|e| e.to_string())?;
        }
        let _ = owned
            .player
            .set_audio_renderer_info(AudioStreamUsage::Music);
        let _ = owned
            .player
            .set_audio_interrupt_mode(AudioInterruptMode::Share);
        Ok(owned)
    }

    fn open_file(
        tx: mpsc::Sender<WorkerEvent>,
        target: Target,
        path: &Path,
    ) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut av = Self::new(tx, target)?;
        if let Err(err) = set_opened_file(&mut av.player, &file) {
            av.close();
            return Err(err);
        }
        Ok(av)
    }

    fn close_inner(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        match self.player.close_sync() {
            Ok(()) => {
                // SAFETY: `close_sync` synchronously stopped the native AVPlayer and drained
                // callbacks, so dropping `player` and then `callbacks` is safe.
                unsafe {
                    std::mem::ManuallyDrop::drop(&mut self.player);
                    std::mem::ManuallyDrop::drop(&mut self.callbacks);
                }
            }
            Err(err) => {
                // Keep both handle and callback box alive if C teardown failed so late
                // native callbacks can never dereference freed memory.
                log::warn!("OHOS audio AVPlayer close failed: {err}");
            }
        }
    }

    fn close(mut self) {
        self.close_inner();
    }
}

impl Drop for OwnedAvPlayer {
    fn drop(&mut self) {
        self.close_inner();
    }
}

struct SoundEntry {
    generation: u64,
    file: Arc<TempAudioFile>,
    max_duration: Duration,
    loaded: bool,
    truncate_after: Option<Duration>,
    probe: Option<OwnedAvPlayer>,
}

struct SoundVoice {
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
    fn duration(&mut self) -> Option<Duration> {
        self.prepared
            .then(|| {
                self.av
                    .as_mut()?
                    .player
                    .duration()
                    .ok()
                    .filter(|&ms| ms > 0)
            })
            .flatten()
            .map(|ms| Duration::from_millis(ms as u64))
    }

    fn apply_settings(&mut self) {
        let Some(av) = self.av.as_mut().filter(|_| self.prepared) else {
            return;
        };
        let v = clamp(self.settings.volume, 0.0, 1.0);
        let _ = av.player.volume(v, v);
        let _ = av.player.looping(self.settings.looped);
        let _ = av.player.playback_rate(clamp(self.settings.rate, 0.5, 2.0));
    }

    fn seek_clamped(&mut self, target_ms: i32) {
        let clamped = match self.duration() {
            Some(d) => target_ms.min((d.as_millis() as i64).min(i64::from(i32::MAX)) as i32),
            None => target_ms,
        };
        if let Some(av) = self.av.as_mut() {
            let _ = av.player.seek(clamped, SeekMode::Closest);
        }
    }

    fn release(&mut self) {
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
    next_gen: u64,
    next_voice_id: u64,
    sounds: FxHashMap<u32, SoundEntry>,
    voices: Vec<SoundVoice>,
    players: FxHashMap<u32, ManagedPlayer>,
}

impl AudioWorker {
    fn emit(&self, id: u32, event: AudioEvent) {
        emit_to(&self.route, id, event);
    }

    fn fail(&self, id: u32, error: AudioError, message: impl Into<String>) {
        self.emit(
            id,
            AudioEvent::Failed {
                error,
                message: message.into(),
            },
        );
    }

    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(self.next_timeout()) {
                Ok(WorkerEvent::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(WorkerEvent::Command(cmd)) => self.handle_command(cmd),
                Ok(WorkerEvent::Notice(Target::Probe(id, g), n)) => self.on_probe(id, g, n),
                Ok(WorkerEvent::Notice(Target::Voice(id), n)) => self.on_voice(id, n),
                Ok(WorkerEvent::Notice(Target::Player(id, g), n)) => self.on_player(id, g, n),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        for v in self.voices.drain(..) {
            v.av.close();
        }
        for (_, mut s) in self.sounds.drain() {
            if let Some(p) = s.probe.take() {
                p.close();
            }
        }
        for (_, mut p) in self.players.drain() {
            p.release();
        }
    }

    fn next_timeout(&self) -> Duration {
        let now = Instant::now();
        let mut wait = Duration::from_secs(60);
        for d in self.voices.iter().filter_map(|v| v.deadline) {
            wait = wait.min(d.saturating_duration_since(now));
        }
        for p in self.players.values().filter(|p| p.playing && p.prepared) {
            wait = wait.min(
                PROGRESS_INTERVAL.saturating_sub(now.saturating_duration_since(p.last_progress)),
            );
        }
        wait
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.drop_voices(|v| v.deadline.is_some_and(|d| now >= d));
        let due: Vec<u32> = self
            .players
            .iter()
            .filter_map(|(&id, p)| {
                (p.playing
                    && p.prepared
                    && now.saturating_duration_since(p.last_progress) >= PROGRESS_INTERVAL)
                    .then_some(id)
            })
            .collect();
        for id in due {
            if let Some(p) = self.players.get_mut(&id).filter(|p| p.prepared) {
                p.last_progress = now;
                let ms =
                    p.av.as_mut()
                        .and_then(|a| a.player.current_time().ok())
                        .unwrap_or(0)
                        .max(0);
                let duration = p.duration();
                self.emit(
                    id,
                    AudioEvent::Progress {
                        position: Duration::from_millis(ms as u64),
                        duration,
                    },
                );
            }
        }
    }

    fn drop_voices(&mut self, mut pred: impl FnMut(&SoundVoice) -> bool) {
        let mut i = 0;
        while i < self.voices.len() {
            if pred(&self.voices[i]) {
                self.voices.remove(i).av.close();
            } else {
                i += 1;
            }
        }
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
                if let Err((err, msg)) = self.load_sound(id, &bytes, &hint, max_duration) {
                    self.fail(id, err, msg);
                }
            }
            AudioCommand::Play { id, volume, rate } => self.play_sound(id, volume, rate),
            AudioCommand::Stop { id } => self.drop_voices(|v| v.sound_id == id),
            AudioCommand::Unload { id } => self.unload_sound(id),
            AudioCommand::Open { id, src, settings } => {
                self.close_player(id);
                self.next_gen += 1;
                self.players.insert(
                    id,
                    ManagedPlayer {
                        generation: self.next_gen,
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
            AudioCommand::Preload { id } => self.ensure_player(id),
            AudioCommand::Resume { id } => {
                let next_gen = self.next_gen + 1;
                let Some(p) = self.players.get_mut(&id) else {
                    return;
                };
                if p.failed {
                    self.next_gen = next_gen;
                    p.generation = next_gen;
                    p.failed = false;
                    p.release();
                }
                if p.playing {
                    return;
                }
                p.wants_play = true;
                if p.prepared
                    && let Some(av) = p.av.as_mut()
                {
                    if let Err(err) = av.player.play() {
                        self.fail_player(id, err.to_string());
                    }
                } else {
                    self.ensure_player(id);
                }
            }
            AudioCommand::Pause { id } => {
                if let Some(p) = self.players.get_mut(&id) {
                    let was = p.playing || p.wants_play;
                    p.wants_play = false;
                    if p.playing
                        && let Some(av) = p.av.as_mut()
                    {
                        let _ = av.player.pause();
                    }
                    p.playing = false;
                    if was {
                        self.emit(id, AudioEvent::Paused);
                    }
                }
            }
            AudioCommand::Seek { id, position } => {
                let ms = (position.as_secs_f64() * 1000.0)
                    .clamp(0.0, f64::from(i32::MAX))
                    .round() as i32;
                if let Some(p) = self.players.get_mut(&id) {
                    if p.prepared {
                        p.seek_clamped(ms);
                    } else {
                        p.pending_seek_ms = Some(ms);
                    }
                }
            }
            AudioCommand::Set { id, settings } => {
                if let Some(p) = self.players.get_mut(&id) {
                    p.settings = settings;
                    p.apply_settings();
                }
            }
            AudioCommand::Close { id } => self.close_player(id),
            _ => {}
        }
    }

    fn unload_sound(&mut self, id: u32) {
        self.drop_voices(|v| v.sound_id == id);
        if let Some(mut entry) = self.sounds.remove(&id)
            && let Some(probe) = entry.probe.take()
        {
            probe.close();
        }
    }

    fn load_sound(
        &mut self,
        id: u32,
        mut bytes: &[u8],
        hint: &str,
        max_duration: Duration,
    ) -> Result<(), (AudioError, String)> {
        let file = Arc::new(
            TempAudioFile::create(&self.host.cache_directory(), "sound", id, hint, &mut bytes)
                .map_err(|e| (AudioError::Failed, e.to_string()))?,
        );
        self.next_gen += 1;
        let generation = self.next_gen;
        let probe =
            OwnedAvPlayer::open_file(self.tx.clone(), Target::Probe(id, generation), &file.0)
                .map_err(|msg| (AudioError::Decode, msg))?;
        self.sounds.insert(
            id,
            SoundEntry {
                generation,
                file,
                max_duration,
                loaded: false,
                truncate_after: None,
                probe: Some(probe),
            },
        );
        Ok(())
    }

    fn play_sound(&mut self, id: u32, volume: f32, rate: f32) {
        let Some(entry) = self.sounds.get(&id).filter(|e| e.loaded) else {
            return;
        };
        let (file, max_duration) = (entry.file.clone(), entry.truncate_after);
        if self.voices.len() >= MAX_VOICES {
            self.voices.remove(0).av.close();
        }
        self.next_voice_id += 1;
        let voice_id = self.next_voice_id;
        let Ok(av) = OwnedAvPlayer::open_file(self.tx.clone(), Target::Voice(voice_id), &file.0)
        else {
            return;
        };
        self.voices.push(SoundVoice {
            voice_id,
            sound_id: id,
            volume: clamp(volume, 0.0, 1.0),
            rate: clamp(rate, 0.5, 2.0),
            deadline: None,
            max_duration,
            _file: file,
            av,
        });
    }

    fn ensure_player(&mut self, id: u32) {
        let Some((src, g)) = self
            .players
            .get(&id)
            .filter(|p| p.av.is_none() && !p.failed)
            .map(|p| (p.src.clone(), p.generation))
        else {
            return;
        };
        match self.open_player_source(id, &src, Target::Player(id, g)) {
            Ok((av, temp)) => {
                if let Some(p) = self.players.get_mut(&id) {
                    p.temp_file = temp;
                    p.av = Some(av);
                } else {
                    av.close();
                }
            }
            Err(msg) => self.fail_player(id, msg),
        }
    }

    fn open_player_source(
        &self,
        id: u32,
        src: &str,
        target: Target,
    ) -> Result<(OwnedAvPlayer, Option<TempAudioFile>), String> {
        let stage =
            |reader: &mut dyn Read| -> Result<(OwnedAvPlayer, Option<TempAudioFile>), String> {
                let temp =
                    TempAudioFile::create(&self.host.cache_directory(), "player", id, src, reader)
                        .map_err(|e| e.to_string())?;
                Ok((
                    OwnedAvPlayer::open_file(self.tx.clone(), target, &temp.0)?,
                    Some(temp),
                ))
            };
        if src.starts_with("data:") {
            let bytes = decode_audio_data_uri(src)?;
            return stage(&mut &bytes[..]);
        }
        if src.starts_with("asset:/") {
            if let Some(file_res) = self.assets.open_file(src) {
                let file = file_res?;
                let mut av = OwnedAvPlayer::new(self.tx.clone(), target)?;
                if let Err(err) = set_opened_file(&mut av.player, &file) {
                    av.close();
                    return Err(err);
                }
                return Ok((av, None));
            }
            let mut reader = self.assets.reader(src)?;
            return stage(&mut reader);
        }
        if let Some(rest) = src.strip_prefix("file://") {
            let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy();
            return Ok((
                OwnedAvPlayer::open_file(self.tx.clone(), target, Path::new(decoded.as_ref()))?,
                None,
            ));
        }
        if src.starts_with('/') {
            return Ok((
                OwnedAvPlayer::open_file(self.tx.clone(), target, Path::new(src))?,
                None,
            ));
        }
        if src.starts_with("http://") || src.starts_with("https://") {
            let mut av = OwnedAvPlayer::new(self.tx.clone(), target)?;
            let url = CString::new(src).map_err(|_| "invalid audio URL".to_owned())?;
            if let Err(err) = av.player.set_url(&url) {
                av.close();
                return Err(err.to_string());
            }
            return Ok((av, None));
        }
        Err(format!("unsupported audio source: {src}"))
    }

    fn fail_player(&mut self, id: u32, message: String) {
        let Some(p) = self.players.get_mut(&id).filter(|p| !p.failed) else {
            return;
        };
        let error = if p.src.starts_with("http://") || p.src.starts_with("https://") {
            AudioError::Network
        } else {
            AudioError::Decode
        };
        p.failed = true;
        p.wants_play = false;
        p.playing = false;
        p.release();
        self.fail(id, error, message);
    }

    fn close_player(&mut self, id: u32) {
        if let Some(mut p) = self.players.remove(&id) {
            p.wants_play = false;
            p.playing = false;
            p.release();
        }
    }

    fn on_probe(&mut self, id: u32, generation: u64, kind: AvNotice) {
        let Some(entry) = self
            .sounds
            .get_mut(&id)
            .filter(|e| e.generation == generation)
        else {
            return;
        };
        match kind {
            AvNotice::State(s) if s == av::STATE_INITIALIZED as i32 => {
                if let Some(probe) = entry.probe.as_mut()
                    && let Err(err) = probe.player.prepare()
                {
                    self.unload_sound(id);
                    self.fail(id, AudioError::Decode, err.to_string());
                }
            }
            AvNotice::State(s) if s == av::STATE_PREPARED as i32 => {
                let ms = entry
                    .probe
                    .as_mut()
                    .and_then(|p| p.player.duration().ok())
                    .unwrap_or(0)
                    .max(0) as u64;
                if let Some(probe) = entry.probe.take() {
                    probe.close();
                }
                let raw = Duration::from_millis(ms);
                let truncated = !entry.max_duration.is_zero() && raw > entry.max_duration;
                let kept = if truncated { entry.max_duration } else { raw };
                entry.loaded = true;
                entry.truncate_after = truncated.then_some(kept);
                let bytes =
                    ((kept.as_secs_f64() * 44_100.0).floor().max(0.0) as u64).saturating_mul(4);
                self.emit(
                    id,
                    AudioEvent::Loaded {
                        duration: kept,
                        bytes,
                        truncated,
                    },
                );
            }
            AvNotice::Error(msg) => {
                self.unload_sound(id);
                self.fail(id, AudioError::Decode, msg);
            }
            _ => {}
        }
    }

    fn on_voice(&mut self, voice_id: u64, kind: AvNotice) {
        let Some(idx) = self.voices.iter().position(|v| v.voice_id == voice_id) else {
            return;
        };
        let close = match kind {
            AvNotice::State(s) if s == av::STATE_INITIALIZED as i32 => {
                self.voices[idx].av.player.prepare().is_err()
            }
            AvNotice::State(s) if s == av::STATE_PREPARED as i32 => {
                let v = &mut self.voices[idx];
                let _ = v.av.player.volume(v.volume, v.volume);
                let _ = v.av.player.playback_rate(v.rate);
                if v.av.player.play().is_err() {
                    true
                } else {
                    if let Some(d) = v.max_duration {
                        v.deadline = Some(
                            Instant::now()
                                + Duration::from_secs_f64(
                                    d.as_secs_f64() / f64::from(v.rate.max(0.25)),
                                ),
                        );
                    }
                    false
                }
            }
            AvNotice::State(s)
                if s == av::STATE_COMPLETED as i32
                    || s == av::STATE_STOPPED as i32
                    || s == av::STATE_RELEASED as i32 =>
            {
                true
            }
            AvNotice::Error(_) => true,
            _ => false,
        };
        if close {
            self.voices.remove(idx).av.close();
        }
    }

    fn on_player(&mut self, id: u32, generation: u64, kind: AvNotice) {
        let Some(p) = self
            .players
            .get_mut(&id)
            .filter(|p| p.generation == generation && p.av.is_some())
        else {
            return;
        };
        match kind {
            AvNotice::State(s) if s == av::STATE_INITIALIZED as i32 => {
                if let Err(err) = p.av.as_mut().unwrap().player.prepare() {
                    self.fail_player(id, err.to_string());
                }
            }
            AvNotice::State(s) if s == av::STATE_PREPARED as i32 => {
                p.prepared = true;
                p.apply_settings();
                let duration = p.duration();
                if let Some(ms) = p.pending_seek_ms.take() {
                    p.seek_clamped(ms);
                }
                let play_err = p
                    .wants_play
                    .then(|| p.av.as_mut().unwrap().player.play().err())
                    .flatten();
                self.emit(id, AudioEvent::Load { duration });
                if let Some(err) = play_err {
                    self.fail_player(id, err.to_string());
                }
            }
            AvNotice::State(s) if s == av::STATE_PLAYING as i32 => {
                if !p.wants_play {
                    let _ = p.av.as_mut().unwrap().player.pause();
                    p.playing = false;
                    return;
                }
                p.playing = true;
                p.last_progress = Instant::now();
                p.apply_settings();
                self.emit(id, AudioEvent::Playing);
            }
            AvNotice::State(s) if s == av::STATE_PAUSED as i32 => {
                if std::mem::replace(&mut p.playing, false) {
                    self.emit(id, AudioEvent::Paused);
                }
            }
            AvNotice::State(s) if s == av::STATE_COMPLETED as i32 => {
                if p.settings.looped {
                    return;
                }
                p.playing = false;
                p.wants_play = false;
                if let Some(d) = p.duration() {
                    self.emit(
                        id,
                        AudioEvent::Progress {
                            position: d,
                            duration: Some(d),
                        },
                    );
                }
                self.emit(id, AudioEvent::Ended);
            }
            AvNotice::SeekDone(pos_ms) => {
                if !p.prepared {
                    return;
                }
                let ms = if pos_ms >= 0 {
                    pos_ms
                } else {
                    p.av.as_mut()
                        .and_then(|a| a.player.current_time().ok())
                        .unwrap_or(0)
                        .max(0)
                };
                let duration = p.duration();
                self.emit(
                    id,
                    AudioEvent::Progress {
                        position: Duration::from_millis(ms as u64),
                        duration,
                    },
                );
            }
            AvNotice::Buffering(k)
                if k == av::BUFFERING_START as i32 || k == av::BUFFERING_END as i32 =>
            {
                self.emit(
                    id,
                    AudioEvent::Buffer {
                        buffering: k == av::BUFFERING_START as i32,
                    },
                );
            }
            AvNotice::Error(msg) => self.fail_player(id, msg),
            _ => {}
        }
    }
}

fn decode_audio_data_uri(uri: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let (header, payload) = uri.split_once(',').ok_or("invalid audio data URI")?;
    let bytes = if header.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| format!("invalid base64 audio data URI: {e}"))?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };
    if bytes.is_empty() {
        return Err("empty audio data URI".into());
    }
    Ok(bytes)
}

/// Per-surface audio controller. Lazily spawns one worker thread on first audio
/// command and tears down all players and staged temp files on close.
pub(crate) struct Audio {
    host: Arc<Services>,
    route: Weak<Dispatcher>,
    assets: Arc<dyn StreamAssets>,
    tx: RefCell<Option<mpsc::Sender<WorkerEvent>>>,
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
            tx: RefCell::new(None),
        }
    }

    pub(crate) fn command(&self, command: AudioCommand) -> napi_ohos::Result<()> {
        if let AudioCommand::LoadUrl { id, .. } = command {
            emit_to(
                &self.route,
                id,
                AudioEvent::Failed {
                    error: AudioError::NotSupported,
                    message: "LoadUrl is not supported on native platforms".into(),
                },
            );
            return Ok(());
        }
        if self.tx.borrow().is_none() {
            let (tx, rx) = mpsc::channel();
            let worker = AudioWorker {
                host: self.host.clone(),
                route: self.route.clone(),
                assets: self.assets.clone(),
                tx: tx.clone(),
                rx,
                next_gen: 0,
                next_voice_id: 0,
                sounds: FxHashMap::default(),
                voices: Vec::new(),
                players: FxHashMap::default(),
            };
            std::thread::Builder::new()
                .name("nsh-audio".into())
                .spawn(move || worker.run())
                .map_err(|e| napi_ohos::Error::from_reason(e.to_string()))?;
            *self.tx.borrow_mut() = Some(tx);
        }
        if let Some(tx) = self.tx.borrow().as_ref() {
            let _ = tx.send(WorkerEvent::Command(command));
        }
        Ok(())
    }

    pub(crate) fn close(&self) {
        if let Some(tx) = self.tx.borrow_mut().take() {
            let _ = tx.send(WorkerEvent::Shutdown);
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.close();
    }
}
