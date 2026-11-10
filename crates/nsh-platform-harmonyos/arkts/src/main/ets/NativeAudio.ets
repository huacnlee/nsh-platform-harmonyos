import { SystemReply, SystemRequest } from "./NativeSystem";
import { BusinessError } from "@ohos.base";
import fs from "@ohos.file.fs";
import media from "@ohos.multimedia.media";

const MAX_VOICES: number = 32;
const PROGRESS_MS: number = 250;

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
    path: string,
    volume: number,
    rate: number,
    onDone: (voice: SoundVoice) => void,
  ) {
    this.soundId = soundId;
    this.onDone = onDone;
    this.start(path, volume, rate);
  }

  private async start(path: string, volume: number, rate: number): Promise<void> {
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
  probe?: media.AVPlayer;
  probeFd?: number;
}

class ManagedPlayer {
  readonly id: number;
  readonly key: number;
  private readonly src: string;
  private readonly tempFile: boolean;
  private readonly openError?: string;
  private readonly reply: (event: SystemReply) => void;
  private volume: number;
  private rate: number;
  private looped: boolean;
  private player?: media.AVPlayer;
  private fd?: number;
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
    if (p) {
      p.release()
        .catch(() => {})
        .finally(() => closeQuietly(fd));
    } else {
      closeQuietly(fd);
    }
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
    if (this.player || this.failed || this.closed) {
      return;
    }
    if (this.openError) {
      this.fail(this.openError);
      return;
    }
    try {
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
      if (this.isHttp()) {
        p.url = this.src;
      } else {
        const file = fs.openSync(this.src, fs.OpenMode.READ_ONLY);
        this.fd = file.fd;
        p.fdSrc = { fd: file.fd };
      }
    } catch (error) {
      this.fail(String(error));
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
  private readonly sounds: Map<number, SoundEntry> = new Map();
  private readonly voices: SoundVoice[] = [];
  private readonly players: Map<number, ManagedPlayer> = new Map();

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
    };
    this.sounds.set(id, entry);
    try {
      const file = fs.openSync(path, fs.OpenMode.READ_ONLY);
      entry.probeFd = file.fd;
      const p = await media.createAVPlayer();
      if (this.sounds.get(id) !== entry) {
        await p.release();
        closeQuietly(file.fd);
        unlinkQuietly(path);
        return;
      }
      entry.probe = p;
      const releaseProbe = (): void => {
        const probe = entry.probe;
        entry.probe = undefined;
        const fd = entry.probeFd;
        entry.probeFd = undefined;
        if (probe) {
          probe
            .release()
            .catch(() => {})
            .finally(() => closeQuietly(fd));
        } else {
          closeQuietly(fd);
        }
      };
      p.on("stateChange", (state: string) => {
        if (this.sounds.get(id) !== entry || entry.probe !== p) {
          return;
        }
        if (state === "initialized") {
          p.prepare().catch((err: BusinessError) => {
            releaseProbe();
            this.sounds.delete(id);
            unlinkQuietly(entry.path);
            entry.path = undefined;
            reply({
              phase: "failed",
              code: "decode",
              error: err.message ?? String(err),
            });
          });
        } else if (state === "prepared") {
          const durationMs = Math.max(0, p.duration);
          releaseProbe();
          const truncated = maxDurationMs > 0 && durationMs > maxDurationMs;
          const keptMs = maxDurationMs > 0 ? Math.min(durationMs, maxDurationMs) : durationMs;
          const bytes = Math.floor((keptMs * 44100) / 1000) * 4;
          if (truncated) {
            unlinkQuietly(entry.path);
            entry.path = undefined;
            reply({
              phase: "loaded",
              durationMs: keptMs,
              bytes,
              truncated: true,
            });
            return;
          }
          entry.loaded = true;
          reply({
            phase: "loaded",
            durationMs: keptMs,
            bytes,
            truncated: false,
          });
        }
      });
      p.on("error", (err: BusinessError) => {
        if (this.sounds.get(id) !== entry || entry.probe !== p) {
          return;
        }
        releaseProbe();
        this.sounds.delete(id);
        unlinkQuietly(entry.path);
        entry.path = undefined;
        reply({
          phase: "failed",
          code: "decode",
          error: err.message ?? String(err),
        });
      });
      p.fdSrc = { fd: file.fd };
    } catch (error) {
      this.unloadSound(id);
      reply({
        phase: "failed",
        code: "decode",
        error: String(error),
      });
    }
  }

  playSound(id: number, volume: number, rate: number): void {
    const entry = this.sounds.get(id);
    if (!entry || !entry.loaded || !entry.path) {
      return;
    }
    while (this.voices.length >= MAX_VOICES) {
      const oldest = this.voices.shift();
      oldest?.stop();
    }
    const voice = new SoundVoice(id, entry.path, volume, rate, (done: SoundVoice) => {
      const index = this.voices.indexOf(done);
      if (index >= 0) {
        this.voices.splice(index, 1);
      }
    });
    this.voices.push(voice);
  }

  stopSound(id: number): void {
    const active = this.voices.filter((v) => v.soundId === id);
    for (const voice of active) {
      voice.stop();
    }
  }

  unloadSound(id: number): void {
    this.stopSound(id);
    const entry = this.sounds.get(id);
    if (!entry) {
      return;
    }
    this.sounds.delete(id);
    const probe = entry.probe;
    entry.probe = undefined;
    const fd = entry.probeFd;
    entry.probeFd = undefined;
    const path = entry.path;
    entry.path = undefined;
    if (probe) {
      probe
        .release()
        .catch(() => {})
        .finally(() => {
          closeQuietly(fd);
          unlinkQuietly(path);
        });
    } else {
      closeQuietly(fd);
      unlinkQuietly(path);
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
    for (const id of Array.from(this.players.keys())) {
      this.closePlayer(id);
    }
    for (const id of Array.from(this.sounds.keys())) {
      this.unloadSound(id);
    }
  }
}
