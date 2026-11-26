import { SystemReply } from "./NativeSystem";
import { abilityAccessCtrl, common } from "@kit.AbilityKit";
import audio from "@ohos.multimedia.audio";
import i18n from "@ohos.i18n";
import type speechRecognizer from "@hms.ai.speechRecognizer";

export interface SpeechReply extends SystemReply {
  phase: string;
  text?: string;
  final?: boolean;
  error?: string;
}

const STOP_TIMEOUT_MS = 3000;

interface ErrorCodeCarrier {
  code?: number;
}

function extractErrorCode(error: Object | undefined | null): number | undefined {
  if (error === undefined || error === null || typeof error !== "object") {
    return undefined;
  }
  const code = (error as ErrorCodeCarrier).code;
  return typeof code === "number" && Number.isFinite(code) ? code : undefined;
}

function mapHmsSpeechError(code?: number): string {
  switch (code) {
    case 201:
    case 202:
    case 1002200012:
      return "not-allowed";
    case 1002200006:
    case 1002200008:
      return "unavailable";
    default:
      return "failed";
  }
}

class Recognition {
  readonly key: number;
  readonly reply: (event: SpeechReply) => void;
  readonly scheduleRelease: (work: () => Promise<void>) => void;
  engine?: speechRecognizer.SpeechRecognitionEngine;
  microphone?: audio.AudioCapturer;
  ended: boolean = false;
  stopping: boolean = false;
  listening: boolean = false;
  pending: Uint8Array = new Uint8Array(0);
  committed: string = "";
  draft: string = "";
  lastInterimText: string = "";
  stopTimer?: number;

  constructor(
    key: number,
    reply: (event: SpeechReply) => void,
    scheduleRelease: (work: () => Promise<void>) => void,
  ) {
    this.key = key;
    this.reply = reply;
    this.scheduleRelease = scheduleRelease;
  }

  applyResult(result: speechRecognizer.SpeechRecognitionResult): string {
    const clause = result.result ?? "";
    if (result.isFinal) {
      this.committed += clause;
      this.draft = "";
    } else {
      this.draft = clause;
    }
    return this.fullTranscript();
  }

  fullTranscript(): string {
    return this.committed + this.draft;
  }

  clearStopTimer(): void {
    if (this.stopTimer !== undefined) {
      clearTimeout(this.stopTimer);
      this.stopTimer = undefined;
    }
  }

  async release(): Promise<void> {
    this.clearStopTimer();
    const microphone = this.microphone;
    this.microphone = undefined;
    if (microphone) {
      try {
        microphone.off("readData");
        await microphone.stop();
      } catch {
      } finally {
        await microphone.release();
      }
    }
    const engine = this.engine;
    this.engine = undefined;
    if (engine) {
      try {
        engine.cancel(String(this.key));
      } catch {
      } finally {
        engine.shutdown();
      }
    }
    this.pending = new Uint8Array(0);
  }

  end(error?: string, silent: boolean = false): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.clearStopTimer();
    if (!silent) {
      this.reply({ phase: "end", error });
    }
    this.scheduleRelease((): Promise<void> => this.release());
  }
}

export class NativeSpeech {
  private readonly context: common.UIAbilityContext;
  private current?: Recognition;
  private teardown: Promise<void> = Promise.resolve();
  private closed: boolean = false;

  constructor(context: common.UIAbilityContext) {
    this.context = context;
  }

  private enqueueRelease(work: () => Promise<void>): void {
    this.teardown = this.teardown
      .then(work)
      .catch((reason: Error) => console.error(`NSH speech release: ${reason}`));
  }

  async start(
    key: number,
    language: string | undefined,
    interim: boolean,
    reply: (event: SpeechReply) => void,
  ): Promise<void> {
    this.current?.end();
    const waitForPrev = this.teardown;
    const session = new Recognition(key, reply, (work: () => Promise<void>): void =>
      this.enqueueRelease(work),
    );
    this.current = session;
    const live = (): boolean => !this.closed && !session.ended && this.current === session;
    try {
      await waitForPrev;
      if (!live()) {
        return;
      }
      if (!canIUse("SystemCapability.AI.SpeechRecognizer")) {
        session.end("unavailable");
        return;
      }
      const granted = await abilityAccessCtrl
        .createAtManager()
        .requestPermissionsFromUser(this.context, ["ohos.permission.MICROPHONE"]);
      if (!live()) {
        return;
      }
      if (granted.authResults.some((value: number) => value !== 0)) {
        session.end("not-allowed");
        return;
      }
      if (session.stopping) {
        session.end();
        return;
      }
      const requested = language || i18n.System.getSystemLanguage();
      const lower = requested.toLowerCase();
      const locale = ["zh", "zh-hans", "zh-hans-cn", "zh-cn"].includes(lower) ? "zh-CN" : requested;
      let engine: speechRecognizer.SpeechRecognitionEngine | undefined = undefined;
      try {
        const recognizer = await import("@hms.ai.speechRecognizer");
        if (!live()) {
          return;
        }
        try {
          engine = await recognizer.default.createEngine({ language: locale, online: 1 });
        } catch (error) {
          if (live()) {
            console.warn(`NSH speech createEngine: ${String(error)}`);
            session.end(mapHmsSpeechError(extractErrorCode(error as Object)));
          }
          return;
        }
      } catch (error) {
        if (live()) {
          console.warn(`NSH speech import: ${String(error)}`);
          session.end("unavailable");
        }
        return;
      }
      if (engine === undefined) {
        return;
      }
      const activeEngine: speechRecognizer.SpeechRecognitionEngine = engine;
      if (!live()) {
        activeEngine.shutdown();
        return;
      }
      session.engine = activeEngine;
      const supported = await activeEngine.listLanguages({ sessionId: String(key) });
      if (!live()) {
        return;
      }
      if (
        supported.length > 0 &&
        !supported.some((s: string) => s.toLowerCase() === locale.toLowerCase())
      ) {
        session.end("language-not-supported");
        return;
      }
      activeEngine.setListener({
        onStart: (_id: string, _message: string): void => {},
        onEvent: (_id: string, _code: number, _message: string): void => {},
        onResult: (_id: string, result: speechRecognizer.SpeechRecognitionResult): void => {
          if (!live()) {
            return;
          }
          const text = session.applyResult(result);
          if (interim && text.length > 0 && text !== session.lastInterimText) {
            session.lastInterimText = text;
            reply({ phase: "result", text, final: false });
          }
        },
        onComplete: (_id: string, _message: string): void => {
          if (!live()) {
            return;
          }
          const text = session.fullTranscript();
          if (text.length > 0) {
            reply({ phase: "result", text, final: true });
          }
          session.end();
        },
        onError: (_id: string, code: number, message: string): void => {
          if (live()) {
            console.warn(`NSH speech ${code}: ${message}`);
            session.end(mapHmsSpeechError(code));
          }
        },
      });
      const microphone = await audio.createAudioCapturer({
        streamInfo: {
          samplingRate: audio.AudioSamplingRate.SAMPLE_RATE_16000,
          channels: audio.AudioChannel.CHANNEL_1,
          sampleFormat: audio.AudioSampleFormat.SAMPLE_FORMAT_S16LE,
          encodingType: audio.AudioEncodingType.ENCODING_TYPE_RAW,
        },
        capturerInfo: { source: audio.SourceType.SOURCE_TYPE_MIC, capturerFlags: 0 },
      });
      if (!live()) {
        await microphone.release();
        return;
      }
      session.microphone = microphone;
      if (session.stopping) {
        session.end();
        return;
      }
      activeEngine.startListening({
        sessionId: String(key),
        audioInfo: { audioType: "pcm", sampleRate: 16000, soundChannel: 1, sampleBit: 16 },
      });
      session.listening = true;
      microphone.on("readData", (buffer: ArrayBuffer): void => {
        if (!live() || session.stopping) {
          return;
        }
        const incoming = new Uint8Array(buffer);
        const bytes = new Uint8Array(session.pending.length + incoming.length);
        bytes.set(session.pending);
        bytes.set(incoming, session.pending.length);
        let offset = 0;
        try {
          for (; offset + 640 <= bytes.length; offset += 640) {
            activeEngine.writeAudio(String(key), bytes.slice(offset, offset + 640));
          }
          session.pending = bytes.slice(offset);
        } catch (error) {
          console.warn(`NSH speech audio: ${error}`);
          session.end("audio-capture");
        }
      });
      await microphone.start();
      if (!live()) {
        await session.release();
        return;
      }
      reply({ phase: "start" });
      if (session.stopping) {
        await this.finishListening(session, key);
      }
    } catch (error) {
      if (live()) {
        console.warn(`NSH speech start: ${String(error)}`);
        const code = extractErrorCode(error as Object);
        session.end(code !== undefined ? mapHmsSpeechError(code) : "failed");
      }
    }
  }

  private async finishListening(session: Recognition, key: number): Promise<void> {
    const microphone = session.microphone;
    const engine = session.engine;
    if (!microphone || !engine || !session.listening) {
      return;
    }
    try {
      microphone.off("readData");
      await microphone.stop();
      if (!session.ended && session.engine === engine) {
        engine.finish(String(key));
        session.stopTimer = setTimeout((): void => {
          if (!this.closed && !session.ended && this.current === session) {
            session.end();
          }
        }, STOP_TIMEOUT_MS);
      }
    } catch (error) {
      if (!session.ended) {
        console.warn(`NSH speech stop: ${error}`);
        session.end("failed");
      }
    }
  }

  async stop(key: number, abort: boolean): Promise<void> {
    const session = this.current;
    if (!session || session.key !== key || session.ended) {
      return;
    }
    if (abort) {
      session.end(undefined, true);
      return;
    }
    if (session.stopping) {
      return;
    }
    session.stopping = true;
    if (!session.listening) {
      return;
    }
    await this.finishListening(session, key);
  }

  close(): void {
    this.closed = true;
    this.current?.end("audio-capture");
    this.current = undefined;
  }
}
