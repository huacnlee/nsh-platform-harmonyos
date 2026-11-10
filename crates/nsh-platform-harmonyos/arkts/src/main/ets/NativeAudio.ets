// Short sounds share a decoded pool; AVPlayer preserves the other rates and
// handles formats or sizes the pool cannot play in full.
import { SystemReply, SystemRequest } from "./NativeSystem";
import { common } from "@kit.AbilityKit";
import { BusinessError } from "@ohos.base";
import fs from "@ohos.file.fs";
import audio from "@ohos.multimedia.audio";
import media from "@ohos.multimedia.media";

const MAX_VOICES: number = 32;
const PROGRESS_MS: number = 250;
const MAX_POOL_BYTES: number = 1000000;

function mapPlaybackSpeed(rate: number): media.PlaybackSpeed {
  if (rate < 0.625) {
    return media.PlaybackSpeed.SPEED_FORWARD_0_50_X;
  }
  if (rate < 0.875) {
    return media.PlaybackSpeed.SPEED_FORWARD_0_75_X;
  }
  if (rate < 1.125) {
    return media.PlaybackSpeed.SPEED_FORWARD_1_00_X;
  }
  if (rate < 1.375) {
    return media.PlaybackSpeed.SPEED_FORWARD_1_25_X;
  }
  if (rate < 1.625) {
    return media.PlaybackSpeed.SPEED_FORWARD_1_50_X;
  }
  if (rate < 1.875) {
    return media.PlaybackSpeed.SPEED_FORWARD_1_75_X;
  }
  return media.PlaybackSpeed.SPEED_FORWARD_2_00_X;
}

function mapPoolRate(rate: number): audio.AudioRendererRate | undefined {
  switch (mapPlaybackSpeed(rate)) {
    case media.PlaybackSpeed.SPEED_FORWARD_0_50_X:
      return audio.AudioRendererRate.RENDER_RATE_HALF;
    case media.PlaybackSpeed.SPEED_FORWARD_1_00_X:
      return audio.AudioRendererRate.RENDER_RATE_NORMAL;
    case media.PlaybackSpeed.SPEED_FORWARD_2_00_X:
      return audio.AudioRendererRate.RENDER_RATE_DOUBLE;
    default:
      return undefined;
  }
}

function fitsSoundPool(metadata: media.AVMetadata, duration: number, sampleRate: number): boolean {
  // A short sound fits the pool's 1 MB decoded limit even at the worst common
  // format (96 kHz stereo 32-bit float), and metadata may omit the track keys.
  // Longer sounds need the metadata's actual rate and channels.
  if (Number.isFinite(duration) && duration > 0 && duration <= 1000) {
    return true;
  }
  // SoundPool silently truncates decoded data at 1 MB. Unknown channel counts
  // or sample rates stay on AVPlayer; the reported stereo budget is no proof.
  if (!Number.isFinite(sampleRate) || sampleRate <= 0 ||
    Math.ceil((duration * sampleRate) / 1000) * 4 >= MAX_POOL_BYTES) {
    return false;
  }
  const tracks = metadata.tracks ?? [];
  const audioTracks = tracks.filter((track) =>
    track[media.MediaDescriptionKey.MD_KEY_TRACK_TYPE] === 1);
  if (audioTracks.length !== 1) {
    return false;
  }
  const channels = audioTracks[0][media.MediaDescriptionKey.MD_KEY_AUD_CHANNEL_COUNT];
  return channels === 1 || channels === 2;
}

function unlinkQuietly(path?: string): void {
  if (!path) {
    return;
  }
  try {
    fs.unlinkSync(path);
  } catch {}
}

function closeQuietly(fd?: number): void {
  if (fd === undefined) {
    return;
  }
  try {
    fs.closeSync(fd);
  } catch {}
}

class SoundVoice {
  readonly soundId: number;
  private player?: media.AVPlayer;
  private fd?: number;
  private closed: boolean = false;
  private readonly onDone: (voice: SoundVoice) => void;

  constructor(
    soundId: number,
    onDone: (voice: SoundVoice) => void,
  ) {
    this.soundId = soundId;
    this.onDone = onDone;
  }

  async start(path: string, volume: number, rate: number): Promise<void> {
    try {
      const file = fs.openSync(path, fs.OpenMode.READ_ONLY);
      this.fd = file.fd;
      const p = await media.createAVPlayer();
      if (this.closed) {
        await p.release();
        closeQuietly(this.fd);
        this.fd = undefined;
        return;
      }
      this.player = p;
      const v = Math.max(0, Math.min(1, volume));
      const speed = mapPlaybackSpeed(Math.max(0.5, Math.min(2, rate)));
      p.on("stateChange", (state: string) => {
        if (this.closed || this.player !== p) {
          return;
        }
        if (state === "initialized") {
          p.prepare().catch(() => this.stop());
        } else if (state === "prepared") {
          p.setVolume(v);
          p.setSpeed(speed);
          p.play().catch(() => this.stop());
        } else if (state === "completed" || state === "stopped" || state === "released") {
          this.stop();
        }
      });
      p.on("error", () => {
        this.stop();
      });
      p.fdSrc = { fd: file.fd };
    } catch {
      this.stop();
    }
  }

  stop(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    const p = this.player;
    this.player = undefined;
    const fd = this.fd;
    this.fd = undefined;
    if (p) {
      p.release()
        .catch(() => {})
        .finally(() => closeQuietly(fd));
    } else {
      closeQuietly(fd);
    }
    this.onDone(this);
  }
}

interface SoundEntry {
  id: number;
  key: number;
  path?: string;
  loaded: boolean;
  replied: boolean;
  extractor?: media.AVMetadataExtractor;
  probe?: media.AVPlayer;
  probeFd?: number;
  poolId?: number;
  poolReady: boolean;
  poolFd?: number;
}

interface PoolVoice {
  kind: "pool";
  soundId: number;
  // Zero reserves a slot while play() is pending, so stop/cap also cancel it.
  stream: number;
}

interface AVVoice {
  kind: "av";
  soundId: number;
  voice: SoundVoice;
}

type Voice = PoolVoice | AVVoice;

class ManagedPlayer {
  readonly id: number;
  readonly key: number;
  private readonly src: string;
  private readonly asset?: string;
  private readonly context: common.UIAbilityContext;
  private readonly tempFile: boolean;
  private readonly openError?: string;
  private readonly reply: (event: SystemReply) => void;
  private volume: number;
  private rate: number;
  private looped: boolean;
  private player?: media.AVPlayer;
  private fd?: number;
  private rawFdName?: string;
  private releasing: Promise<void> = Promise.resolve();
  private opening: boolean = false;
  private prepared: boolean = false;
  private playing: boolean = false;
  private wantsPlay: boolean = false;
  private failed: boolean = false;
  private closed: boolean = false;
  private pendingSeek: number = -1;
  private timer?: number;

  constructor(
    id: number,
    key: number,
    src: string,
    asset: string | undefined,
    context: common.UIAbilityContext,
    tempFile: boolean,
    openError: string | undefined,
    volume: number,
    rate: number,
    looped: boolean,
    reply: (event: SystemReply) => void,
  ) {
    this.id = id;
    this.key = key;
    this.src = src;
    this.asset = asset;
    this.context = context;
    this.tempFile = tempFile;
    this.openError = openError;
    this.volume = volume;
    this.rate = rate;
    this.looped = looped;
    this.reply = reply;
  }

  private isHttp(): boolean {
    return this.src.startsWith("http://") || this.src.startsWith("https://");
  }

  private duration(): number | undefined {
    const d = this.player?.duration ?? -1;
    return this.prepared && d > 0 ? d : undefined;
  }

  private stopTimer(): void {
    if (this.timer !== undefined) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  }

  private startTimer(): void {
    this.stopTimer();
    this.timer = setInterval(() => {
      if (this.closed || !this.playing || !this.player) {
        this.stopTimer();
        return;
      }
      this.progress();
    }, PROGRESS_MS);
  }

  private progress(): void {
    if (!this.player || !this.prepared) {
      return;
    }
    const pos = Math.max(0, this.player.currentTime);
    this.reply({
      phase: "progress",
      positionMs: pos,
      durationMs: this.duration(),
    });
  }

  private fail(message: string): void {
    if (this.failed || this.closed) {
      return;
    }
    this.failed = true;
    this.wantsPlay = false;
    this.playing = false;
    this.stopTimer();
    this.releasePlayer();
    this.reply({
      phase: "failed",
      code: this.isHttp() ? "network" : "decode",
      error: message,
    });
  }

  private releasePlayer(): void {
    this.prepared = false;
    const p = this.player;
    this.player = undefined;
    const fd = this.fd;
    this.fd = undefined;
    const rawFdName = this.rawFdName;
    this.rawFdName = undefined;
    this.releasing = this.releasing.then(async () => {
      if (p) {
        await p.release().catch(() => {});
      }
      closeQuietly(fd);
      if (rawFdName) {
        await this.context.resourceManager.closeRawFd(rawFdName).catch(() => {});
      }
    });
  }

  private applySettings(): void {
    if (!this.player || !this.prepared) {
      return;
    }
    try {
      this.player.setVolume(Math.max(0, Math.min(1, this.volume)));
      this.player.loop = this.looped;
      this.player.setSpeed(mapPlaybackSpeed(Math.max(0.5, Math.min(2, this.rate))));
    } catch {}
  }

  private async ensure(): Promise<void> {
    if (this.player || this.opening || this.failed || this.closed) {
      return;
    }
    if (this.openError) {
      this.fail(this.openError);
      return;
    }
    this.opening = true;
    try {
      await this.releasing;
      if (this.closed || this.failed) {
        return;
      }
      const p = await media.createAVPlayer();
      if (this.closed || this.failed) {
        await p.release();
        return;
      }
      this.player = p;
      p.on("stateChange", (state: string) => {
        if (this.closed || this.player !== p) {
          return;
        }
        if (state === "initialized") {
          p.prepare().catch((err: BusinessError) => this.fail(err.message ?? String(err)));
        } else if (state === "prepared") {
          this.prepared = true;
          this.applySettings();
          this.reply({
            phase: "load",
            durationMs: this.duration(),
          });
          if (this.pendingSeek >= 0) {
            const target = this.pendingSeek;
            this.pendingSeek = -1;
            this.seek(target);
          }
          if (this.wantsPlay) {
            p.play().catch((err: BusinessError) => this.fail(err.message ?? String(err)));
          }
        } else if (state === "playing") {
          this.playing = true;
          this.applySettings();
          this.reply({ phase: "playing" });
          this.startTimer();
        } else if (state === "paused") {
          this.stopTimer();
          if (this.playing) {
            this.playing = false;
            this.reply({ phase: "paused" });
          }
        } else if (state === "completed") {
          if (this.looped) {
            return;
          }
          this.playing = false;
          this.wantsPlay = false;
          this.stopTimer();
          const d = this.duration();
          if (d !== undefined) {
            this.reply({
              phase: "progress",
              positionMs: d,
              durationMs: d,
            });
          }
          this.reply({ phase: "ended" });
        }
      });
      p.on("seekDone", (seekDoneTime: number) => {
        if (!this.closed && this.player === p && this.prepared) {
          this.reply({
            phase: "progress",
            positionMs: Math.max(0, seekDoneTime),
            durationMs: this.duration(),
          });
        }
      });
      p.on("bufferingUpdate", (infoType: media.BufferingInfoType) => {
        if (this.closed || this.player !== p) {
          return;
        }
        if (infoType === media.BufferingInfoType.BUFFERING_START) {
          this.reply({ phase: "buffer", buffering: true });
        } else if (infoType === media.BufferingInfoType.BUFFERING_END) {
          this.reply({ phase: "buffer", buffering: false });
        }
      });
      p.on("error", (err: BusinessError) => {
        if (this.player === p) {
          this.fail(err.message ?? String(err));
        }
      });
      if (this.asset) {
        const name = this.asset;
        const raw = await this.context.resourceManager.getRawFd(name);
        if (this.closed || this.failed || this.player !== p) {
          // close/fail may have released the player while getRawFd was pending.
          await this.releasing;
          await this.context.resourceManager.closeRawFd(name).catch(() => {});
          return;
        }
        this.rawFdName = name;
        p.fdSrc = { fd: raw.fd, offset: raw.offset, length: raw.length };
      } else if (this.isHttp()) {
        p.url = this.src;
      } else {
        const file = fs.openSync(this.src, fs.OpenMode.READ_ONLY);
        this.fd = file.fd;
        p.fdSrc = { fd: file.fd };
      }
    } catch (error) {
      const err = error as BusinessError;
      this.fail(err.message ?? String(error));
    } finally {
      this.opening = false;
      if (this.wantsPlay && !this.player && !this.failed && !this.closed) {
        this.ensure();
      }
    }
  }

  preload(): void {
    this.ensure();
  }

  resume(): void {
    if (this.closed) {
      return;
    }
    if (this.failed) {
      this.failed = false;
      this.releasePlayer();
    }
    if (this.playing) {
      return;
    }
    this.wantsPlay = true;
    if (this.player && this.prepared) {
      this.player.play().catch((err: BusinessError) => this.fail(err.message ?? String(err)));
    } else {
      this.ensure();
    }
  }

  pause(): void {
    const was = this.playing || this.wantsPlay;
    this.wantsPlay = false;
    this.stopTimer();
    if (this.playing && this.player) {
      this.playing = false;
      this.player.pause().catch(() => {});
      this.reply({ phase: "paused" });
    } else if (was) {
      this.playing = false;
      this.reply({ phase: "paused" });
    }
  }

  seek(ms: number): void {
    const clamped = Math.max(0, ms);
    if (!this.player || !this.prepared) {
      this.pendingSeek = clamped;
      return;
    }
    const d = this.duration();
    const target = d !== undefined ? Math.min(clamped, d) : clamped;
    try {
      this.player.seek(target, media.SeekMode.SEEK_CLOSEST);
    } catch {}
  }

  set(volume: number, rate: number, looped: boolean): void {
    this.volume = volume;
    this.rate = rate;
    this.looped = looped;
    this.applySettings();
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.wantsPlay = false;
    this.playing = false;
    this.stopTimer();
    this.releasePlayer();
    if (this.tempFile) {
      unlinkQuietly(this.src);
    }
  }
}

export class NativeAudio {
  private readonly context: common.UIAbilityContext;
  private readonly sounds: Map<number, SoundEntry> = new Map();
  private readonly voices: Voice[] = [];
  private readonly players: Map<number, ManagedPlayer> = new Map();
  private pool?: media.SoundPool;
  private poolPromise?: Promise<void>;
  // Events may arrive before the corresponding load/play promise resolves.
  private readonly poolLoaded: Set<number> = new Set();
  private readonly poolFinished: Set<number> = new Set();
  private pendingLoads: number = 0;
  private pendingPlays: number = 0;
  private closed: boolean = false;

  constructor(context: common.UIAbilityContext) {
    this.context = context;
  }

  private async createPool(): Promise<void> {
    let pool: media.SoundPool | undefined;
    try {
      const renderer: audio.AudioRendererInfo = {
        usage: audio.StreamUsage.STREAM_USAGE_MUSIC,
        rendererFlags: 0,
      };
      pool = await media.createSoundPool(MAX_VOICES, renderer);
      if (this.closed) {
        await pool.release().catch(() => {});
        return;
      }
      pool.on("loadComplete", (soundId: number) => {
        if (this.closed) {
          return;
        }
        for (const entry of this.sounds.values()) {
          if (entry.poolId === soundId) {
            entry.poolReady = true;
            closeQuietly(entry.poolFd);
            entry.poolFd = undefined;
            return;
          }
        }
        if (this.pendingLoads > 0) {
          this.poolLoaded.add(soundId);
        }
      });
      pool.on("playFinishedWithStreamId", (stream: number) => {
        const index = this.voices.findIndex((v) => v.kind === "pool" && v.stream === stream);
        if (index >= 0) {
          this.voices.splice(index, 1);
        } else if (this.pendingPlays > 0) {
          this.poolFinished.add(stream);
        }
      });
      // Load/play promise failures fall back to AVPlayer. Later output failures
      // are silence, as required by Sound.play's fire-and-forget contract.
      pool.on("error", () => {});
      this.pool = pool;
    } catch {
      pool?.release().catch(() => {});
    }
  }

  private async loadPool(entry: SoundEntry): Promise<void> {
    if (!this.poolPromise) {
      this.poolPromise = this.createPool();
    }
    await this.poolPromise;
    const pool = this.pool;
    if (!pool || this.sounds.get(entry.id) !== entry || !entry.path) {
      return;
    }
    let fd: number | undefined;
    this.pendingLoads++;
    try {
      // SoundPool accepts fd://, not a filesystem path. This descriptor belongs
      // to the pool load alone; AVPlayer voices open their own descriptors.
      fd = fs.openSync(entry.path, fs.OpenMode.READ_ONLY).fd;
      const soundId = await pool.load(`fd://${fd}`);
      if (soundId <= 0) {
        return;
      }
      if (this.sounds.get(entry.id) !== entry || this.pool !== pool) {
        this.poolLoaded.delete(soundId);
        await pool.unload(soundId).catch(() => {});
        return;
      }
      entry.poolId = soundId;
      entry.poolReady = this.poolLoaded.delete(soundId);
      if (!entry.poolReady) {
        entry.poolFd = fd;
        fd = undefined;
      }
      // Keep the staged file for intermediate rates and any pool failure.
    } catch {
      // Loading into the pool is optional; the staged AVPlayer source remains.
    } finally {
      closeQuietly(fd);
      this.pendingLoads--;
      if (this.pendingLoads === 0) {
        this.poolLoaded.clear();
      }
    }
  }

  private async releaseProbe(entry: SoundEntry): Promise<void> {
    const probe = entry.probe;
    const extractor = entry.extractor;
    entry.probe = undefined;
    entry.extractor = undefined;
    const fd = entry.probeFd;
    entry.probeFd = undefined;
    try {
      if (probe) {
        await probe.release().catch(() => {});
      }
      if (extractor) {
        await extractor.release().catch(() => {});
      }
    } finally {
      closeQuietly(fd);
    }
  }

  private soundLoaded(
    entry: SoundEntry,
    durationMs: number,
    sampleRate: number,
    maxDurationMs: number,
    poolEligible: boolean,
    reply: (event: SystemReply) => void,
  ): void {
    if (this.sounds.get(entry.id) !== entry || entry.replied) {
      return;
    }
    entry.replied = true;
    const truncated = maxDurationMs > 0 && durationMs > maxDurationMs;
    const keptMs = maxDurationMs > 0 ? Math.min(durationMs, maxDurationMs) : durationMs;
    // Budget estimate: the sample rate x stereo 16-bit frames, converted from
    // milliseconds. The decoder does not expose its real allocation.
    const bytes = Math.floor((keptMs * sampleRate) / 1000) * 4;
    if (truncated) {
      unlinkQuietly(entry.path);
      entry.path = undefined;
    } else {
      entry.loaded = true;
    }
    reply({ phase: "loaded", durationMs: keptMs, bytes, truncated });
    if (!truncated && poolEligible) {
      // The loaded reply must not wait for decoding into the pool.
      this.loadPool(entry).catch(() => {});
    }
  }

  private soundFailed(entry: SoundEntry, error: string, reply: (event: SystemReply) => void): void {
    if (this.sounds.get(entry.id) !== entry || entry.replied) {
      return;
    }
    entry.replied = true;
    this.unloadSound(entry.id);
    reply({ phase: "failed", code: "decode", error });
  }

  async loadSound(
    id: number,
    key: number,
    path: string,
    maxDurationMs: number,
    reply: (event: SystemReply) => void,
  ): Promise<void> {
    this.unloadSound(id);
    const entry: SoundEntry = {
      id,
      key,
      path,
      loaded: false,
      replied: false,
      poolReady: false,
    };
    this.sounds.set(id, entry);
    try {
      const extractor = await media.createAVMetadataExtractor();
      if (this.sounds.get(id) !== entry) {
        await extractor.release().catch(() => {});
        return;
      }
      entry.extractor = extractor;
      const file = fs.openSync(path, fs.OpenMode.READ_ONLY);
      entry.probeFd = file.fd;
      extractor.fdSrc = { fd: file.fd };
      const metadata = await extractor.fetchMetadata();
      const durationMs = Number(metadata.duration);
      const rate = Number(metadata.sampleRate);
      const sampleRate = Number.isFinite(rate) && rate > 0 ? rate : 44100;
      if (Number.isFinite(durationMs) && durationMs > 0) {
        this.soundLoaded(entry, durationMs, sampleRate, maxDurationMs,
          fitsSoundPool(metadata, durationMs, rate), reply);
        return;
      }
    } catch {
      // Missing metadata API, unsupported format, or an unreadable duration:
      // retain the AVPlayer duration probe and its original acceptance rules.
    } finally {
      await this.releaseProbe(entry);
    }
    if (this.sounds.get(id) !== entry) {
      return;
    }
    try {
      const p = await media.createAVPlayer();
      if (this.sounds.get(id) !== entry) {
        await p.release().catch(() => {});
        return;
      }
      entry.probe = p;
      const file = fs.openSync(path, fs.OpenMode.READ_ONLY);
      entry.probeFd = file.fd;
      p.on("stateChange", (state: string) => {
        if (this.sounds.get(id) !== entry || entry.probe !== p) {
          return;
        }
        if (state === "initialized") {
          p.prepare().catch((err: BusinessError) => {
            this.soundFailed(entry, err.message ?? String(err), reply);
          });
        } else if (state === "prepared") {
          const durationMs = Math.max(0, p.duration);
          this.releaseProbe(entry).catch(() => {});
          // AVMetadataExtractor is not available on every device (the
          // avmetadatahelper service may be missing). SoundPool truncates
          // decoded data at 1 MB; a sound of at most 1 s is safe even at the
          // worst common decoded format (96 kHz stereo 32-bit), so try the
          // pool and fall back to AVPlayer if its load fails.
          const poolEligible = durationMs > 0 && durationMs <= 1000;
          this.soundLoaded(entry, durationMs, 44100, maxDurationMs, poolEligible, reply);
        }
      });
      p.on("error", (err: BusinessError) => {
        if (entry.probe === p) {
          this.soundFailed(entry, err.message ?? String(err), reply);
        }
      });
      p.fdSrc = { fd: file.fd };
    } catch (error) {
      this.soundFailed(entry, String(error), reply);
    }
  }

  private stopVoice(voice: Voice): void {
    const index = this.voices.indexOf(voice);
    if (index >= 0) {
      this.voices.splice(index, 1);
    }
    if (voice.kind === "pool") {
      if (voice.stream > 0) {
        this.pool?.stop(voice.stream).catch(() => {});
      }
    } else {
      voice.voice.stop();
    }
  }

  private playAV(entry: SoundEntry, volume: number, rate: number, index: number): void {
    if (this.sounds.get(entry.id) !== entry || !entry.path) {
      return;
    }
    const voice = new SoundVoice(entry.id, (done: SoundVoice) => {
      const index = this.voices.findIndex((v) => v.kind === "av" && v.voice === done);
      if (index >= 0) {
        this.voices.splice(index, 1);
      }
    });
    const active: AVVoice = { kind: "av", soundId: entry.id, voice };
    this.voices.splice(index, 0, active);
    voice.start(entry.path, volume, rate).catch(() => voice.stop());
  }

  private async playPool(
    pool: media.SoundPool,
    entry: SoundEntry,
    voice: PoolVoice,
    volume: number,
    rate: number,
    poolRate: audio.AudioRendererRate,
  ): Promise<void> {
    this.pendingPlays++;
    try {
      let stream = 0;
      try {
        stream = await pool.play(entry.poolId!, {
          loop: 0, rate: poolRate, leftVolume: volume, rightVolume: volume, priority: 0,
        });
      } catch {}
      const index = this.voices.indexOf(voice);
      if (stream > 0) {
        if (index < 0 || this.pool !== pool) {
          await pool.stop(stream).catch(() => {});
        } else if (this.poolFinished.delete(stream)) {
          this.voices.splice(index, 1);
        } else {
          voice.stream = stream;
        }
      } else if (index >= 0) {
        this.voices.splice(index, 1);
        this.playAV(entry, volume, rate, index);
      }
    } finally {
      this.pendingPlays--;
      if (this.pendingPlays === 0) {
        this.poolFinished.clear();
      }
    }
  }

  playSound(id: number, volume: number, rate: number): void {
    const entry = this.sounds.get(id);
    if (!entry || !entry.loaded || !entry.path) {
      return;
    }
    while (this.voices.length >= MAX_VOICES) {
      this.stopVoice(this.voices[0]);
    }
    const v = Math.max(0, Math.min(1, volume));
    const r = Math.max(0.5, Math.min(2, rate));
    const poolRate = mapPoolRate(r);
    if (this.pool && entry.poolReady && entry.poolId !== undefined && poolRate !== undefined) {
      const voice: PoolVoice = { kind: "pool", soundId: id, stream: 0 };
      this.voices.push(voice);
      this.playPool(this.pool, entry, voice, v, r, poolRate).catch(() => {});
    } else {
      this.playAV(entry, v, r, this.voices.length);
    }
  }

  stopSound(id: number): void {
    const active = this.voices.filter((v) => v.soundId === id);
    for (const voice of active) {
      this.stopVoice(voice);
    }
  }

  unloadSound(id: number): void {
    this.stopSound(id);
    const entry = this.sounds.get(id);
    if (!entry) {
      return;
    }
    this.sounds.delete(id);
    const path = entry.path;
    entry.path = undefined;
    this.releaseProbe(entry).finally(() => unlinkQuietly(path)).catch(() => {});
    const fd = entry.poolFd;
    entry.poolFd = undefined;
    if (this.pool && entry.poolId !== undefined) {
      this.poolLoaded.delete(entry.poolId);
      this.pool.unload(entry.poolId).catch(() => {}).finally(() => closeQuietly(fd));
    } else {
      closeQuietly(fd);
    }
  }

  openPlayer(key: number, request: SystemRequest, reply: (event: SystemReply) => void): void {
    const id = request.count ?? 0;
    this.closePlayer(id);
    this.players.set(
      id,
      new ManagedPlayer(
        id,
        key,
        request.uri ?? "",
        request.asset,
        this.context,
        request.hasOnly ?? false,
        request.text,
        request.volume ?? 1.0,
        request.rate ?? 1.0,
        request.looped ?? false,
        reply,
      ),
    );
  }

  preloadPlayer(id: number): void {
    this.players.get(id)?.preload();
  }

  resumePlayer(id: number): void {
    this.players.get(id)?.resume();
  }

  pausePlayer(id: number): void {
    this.players.get(id)?.pause();
  }

  seekPlayer(id: number, positionMs: number): void {
    this.players.get(id)?.seek(positionMs);
  }

  setPlayer(id: number, volume: number, rate: number, looped: boolean): void {
    this.players.get(id)?.set(volume, rate, looped);
  }

  closePlayer(id: number): void {
    const player = this.players.get(id);
    if (player) {
      this.players.delete(id);
      player.close();
    }
  }

  cancel(key: number): void {
    for (const entry of this.sounds.values()) {
      if (entry.key === key) {
        this.unloadSound(entry.id);
        break;
      }
    }
    for (const player of this.players.values()) {
      if (player.key === key) {
        this.closePlayer(player.id);
        break;
      }
    }
  }

  close(): void {
    this.closed = true;
    for (const id of Array.from(this.players.keys())) {
      this.closePlayer(id);
    }
    for (const id of Array.from(this.sounds.keys())) {
      this.unloadSound(id);
    }
    this.pool?.release().catch(() => {});
    this.pool = undefined;
    this.poolLoaded.clear();
    this.poolFinished.clear();
  }
}
