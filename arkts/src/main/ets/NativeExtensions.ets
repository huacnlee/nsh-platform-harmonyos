import { common, Want } from "@kit.AbilityKit";

export type WantKind = "create" | "new-want";

interface QueuedWant {
  kind: WantKind;
  payload: string;
}

const MAX_QUEUED_WANTS = 16;
const queuedWants: QueuedWant[] = [];
let wantSink: ((kind: WantKind, payload: string) => void) | undefined = undefined;

function sanitizeJsonValue(value: ESObject, depth: number): ESObject {
  if (depth > 8 || value === undefined) {
    return undefined;
  }
  if (value === null) {
    return null;
  }
  const kind = typeof value;
  if (kind === "string" || kind === "boolean") {
    return value;
  }
  if (kind === "number") {
    return Number.isFinite(value as number) ? value : undefined;
  }
  if (Array.isArray(value)) {
    const out: Array<ESObject> = [];
    const items: Array<ESObject> = value as Array<ESObject>;
    for (let i = 0; i < items.length && i < 256; i++) {
      const sanitized: ESObject = sanitizeJsonValue(items[i], depth + 1);
      if (sanitized !== undefined) {
        out.push(sanitized);
      }
    }
    return out;
  }
  if (kind === "object") {
    const out: Record<string, ESObject> = {};
    const keys = Object.keys(value as Object);
    for (let i = 0; i < keys.length && i < 256; i++) {
      const key = keys[i];
      const sanitized: ESObject = sanitizeJsonValue((value as Record<string, ESObject>)[key], depth + 1);
      if (sanitized !== undefined) {
        out[key] = sanitized;
      }
    }
    return out;
  }
  return undefined;
}

export function sanitizeWant(want?: Want): Record<string, ESObject> {
  const out: Record<string, ESObject> = {};
  if (!want) {
    return out;
  }
  if (typeof want.bundleName === "string") {
    out["bundleName"] = want.bundleName;
  }
  if (typeof want.moduleName === "string") {
    out["moduleName"] = want.moduleName;
  }
  if (typeof want.abilityName === "string") {
    out["abilityName"] = want.abilityName;
  }
  if (typeof want.action === "string") {
    out["action"] = want.action;
  }
  if (typeof want.uri === "string") {
    out["uri"] = want.uri;
  }
  if (typeof want.type === "string") {
    out["type"] = want.type;
  }
  if (typeof want.flags === "number" && Number.isFinite(want.flags) && want.flags >= 0) {
    out["flags"] = Math.trunc(want.flags);
  }
  if (Array.isArray(want.entities)) {
    const entities: string[] = [];
    for (let i = 0; i < want.entities.length; i++) {
      if (typeof want.entities[i] === "string") {
        entities.push(want.entities[i]);
      }
    }
    if (entities.length > 0) {
      out["entities"] = entities;
    }
  }
  if (want.parameters && typeof want.parameters === "object") {
    const params: ESObject = sanitizeJsonValue(want.parameters as ESObject, 0);
    if (params && typeof params === "object" && Object.keys(params as Object).length > 0) {
      out["parameters"] = params;
    }
  }
  return out;
}

function reportBusinessError(
  error: ESObject,
  complete: (ok: boolean, payload?: string, code?: number, message?: string) => void,
): void {
  const rawCode: ESObject = error?.code;
  const code = typeof rawCode === "number" && Number.isFinite(rawCode) ? rawCode : undefined;
  const rawMessage: ESObject = error?.message;
  const message =
    typeof rawMessage === "string" && rawMessage.length > 0 ? rawMessage : String(error);
  complete(false, undefined, code, message);
}

export function setAbilityWantSink(sink: (kind: WantKind, payload: string) => void): void {
  wantSink = sink;
  if (queuedWants.length > 0) {
    const drained = queuedWants.splice(0, queuedWants.length);
    for (let i = 0; i < drained.length; i++) {
      try {
        sink(drained[i].kind, drained[i].payload);
      } catch (error) {
        console.warn(`NSH ability want replay failed: ${String(error)}`);
      }
    }
  }
}

/**
 * Forward the host `UIAbility`'s `onCreate` and `onNewWant` callbacks to
 * registered Rust `nshm` observers (`on_ability_want` / `launch_want`).
 */
export function onAbilityWant(want: Want, kind: WantKind = "new-want"): void {
  let payload: string;
  try {
    payload = JSON.stringify(sanitizeWant(want));
  } catch {
    payload = "{}";
  }
  if (wantSink !== undefined) {
    try {
      wantSink(kind, payload);
    } catch (error) {
      console.warn(`NSH ability want delivery failed: ${String(error)}`);
    }
    return;
  }
  if (queuedWants.length >= MAX_QUEUED_WANTS) {
    queuedWants.shift();
  }
  queuedWants.push({ kind, payload });
}

export function startAbilityForResult(
  abilityContext: common.UIAbilityContext,
  wantJson: string,
  complete: (ok: boolean, payload?: string, code?: number, message?: string) => void,
): void {
  let want: Want;
  try {
    want = JSON.parse(wantJson) as Want;
  } catch (error) {
    complete(false, undefined, undefined, `invalid Want JSON: ${String(error)}`);
    return;
  }
  try {
    abilityContext
      .startAbilityForResult(want)
      .then((result: common.AbilityResult): void => {
        const payload = JSON.stringify({
          resultCode: typeof result?.resultCode === "number" ? result.resultCode : 0,
          want: result?.want ? sanitizeWant(result.want) : null,
        });
        complete(true, payload, undefined, undefined);
      })
      .catch((error: ESObject): void => {
        reportBusinessError(error, complete);
      });
  } catch (error) {
    reportBusinessError(error as ESObject, complete);
  }
}
