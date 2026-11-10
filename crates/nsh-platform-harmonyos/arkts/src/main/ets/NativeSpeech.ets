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
class Recognition {
  readonly key: number;
  readonly reply: (event: SpeechReply) => void;
  engine?: speechRecognizer.SpeechRecognitionEngine;
  microphone?: audio.AudioCapturer;
  ended: boolean = false;
  stopping: boolean = false;
  pending: Uint8Array = new Uint8Array(0);
  transcript: string = "";
  constructor(key: number, reply: (event: SpeechReply) => void) {
    this.key = key;
    this.reply = reply;
  }
  async release(): Promise<void> {
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
  end(error?: string): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.reply({ phase: "end", error });
    this.release().catch((reason: Error) => console.error(`NSH speech release: ${reason}`));
  }
}
export class NativeSpeech {
  private readonly context: common.UIAbilityContext;
  private current?: Recognition;
  private closed: boolean = false;
  constructor(context: common.UIAbilityContext) {
    this.context = context;
  }
  async start(
    key: number,
    language: string | undefined,
    interim: boolean,
    reply: (event: SpeechReply) => void,
  ): Promise<void> {
    this.current?.end();
    const session = new Recognition(key, reply);
    this.current = session;
    const live = (): boolean => !this.closed && !session.ended && this.current === session;
    try {
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
      const recognizer = await import("@hms.ai.speechRecognizer");
      if (!live()) {
        return;
      }
      // The SDK's offline engine accepts zh-CN. Normalize the system's
      // language/script spelling, and report other requested languages exactly.
      const requested = language || i18n.System.getSystemLanguage();
      const locale = ["zh", "zh-hans", "zh-hans-cn", "zh-cn"].includes(requested.toLowerCase())
        ? "zh-CN"
        : requested;
      if (locale !== "zh-CN") {
        session.end("language-not-supported");
        return;
      }
      const engine = await recognizer.default.createEngine({ language: locale, online: 1 });
      if (!live()) {
        engine.shutdown();
        return;
      }
      session.engine = engine;
      const supported = await engine.listLanguages({ sessionId: String(key) });
      if (!live()) {
        return;
      }
      if (!supported.includes(locale)) {
        session.end("language-not-supported");
        return;
      }
      engine.setListener({
        onStart: (_id: string, _message: string): void => {},
        onEvent: (_id: string, _code: number, _message: string): void => {},
        onResult: (_id: string, result: speechRecognizer.SpeechRecognitionResult): void => {
          if (!live()) {
            return;
          }
          const text = session.transcript + result.result;
          if (interim || result.isFinal) {
            reply({ phase: "result", text, final: result.isLast });
          }
          if (result.isFinal) {
            session.transcript = text;
          }
        },
        onComplete: (_id: string, _message: string): void => {
          if (live()) {
            session.end();
          }
        },
        onError: (_id: string, code: number, message: string): void => {
          if (live()) {
            console.warn(`NSH speech ${code}: ${message}`);
            session.end(code === 201 ? "not-allowed" : "failed");
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
      engine.startListening({
        sessionId: String(key),
        audioInfo: { audioType: "pcm", sampleRate: 16000, soundChannel: 1, sampleBit: 16 },
      });
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
            engine.writeAudio(String(key), bytes.slice(offset, offset + 640));
          }
          session.pending = bytes.slice(offset);
        } catch (error) {
          console.warn(`NSH speech audio: ${error}`);
          session.end("audio-capture");
        }
      });
      await microphone.start();
      if (live()) {
        reply({ phase: "start" });
      } else {
        await session.release();
      }
    } catch (error) {
      if (live()) {
        console.warn(`NSH speech start: ${error}`);
        session.end("unavailable");
      }
    }
  }
  async stop(key: number, abort: boolean): Promise<void> {
    const session = this.current;
    if (!session || session.key !== key || session.ended) {
      return;
    }
    if (abort) {
      session.end();
      return;
    }
    session.stopping = true;
    if (!session.microphone || !session.engine) {
      return;
    }
    try {
      session.microphone.off("readData");
      await session.microphone.stop();
      if (!session.ended) {
        session.engine?.finish(String(key));
      }
    } catch (error) {
      console.warn(`NSH speech stop: ${error}`);
      session.end("failed");
    }
  }
  close(): void {
    this.closed = true;
    this.current?.end();
    this.current = undefined;
  }
}
