import { common, Want } from "@kit.AbilityKit";

export type WantKind = "create" | "new-want";

interface AbilityWantNative {
  nshAbilityWant(kind: string, payload: string): void;
}

let cachedAbilityWantNative: AbilityWantNative | undefined = undefined;

function getAbilityWantNative(): AbilityWantNative {
  if (cachedAbilityWantNative !== undefined) {
    return cachedAbilityWantNative;
  }
  const libraryName = "libnsh_platform_harmonyos.so";
  let loaded: ESObject;
  try {
    loaded = loadNativeModule(libraryName);
  } catch (error) {
    throw new Error(`${libraryName} failed to load: ${String(error)}`);
  }
  const candidate: ESObject =
    typeof loaded?.nshAbilityWant === "function" ? loaded : loaded?.default;
  const mod = candidate as AbilityWantNative;
  if (!mod || typeof mod.nshAbilityWant !== "function") {
    throw new Error(`${libraryName} does not export nshAbilityWant`);
  }
  cachedAbilityWantNative = mod;
  return mod;
}

function jsonReplacer(_key: string, value: ESObject): ESObject {
  if (typeof value === "bigint") {
    return undefined;
  }
  if (typeof value === "number" && !Number.isFinite(value as number)) {
    return undefined;
  }
  return value;
}

function serializeParameters(
  parameters?: Record<string, Object>,
): Record<string, ESObject> | undefined {
  if (!parameters || typeof parameters !== "object") {
    return undefined;
  }
  try {
    const json = JSON.stringify(parameters, jsonReplacer);
    if (!json || json === "{}") {
      return undefined;
    }
    const parsed: ESObject = JSON.parse(json);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as Record<string, ESObject>;
    }
    return undefined;
  } catch {
    // Fallback when one system entry is cyclic or has a throwing getter: keep
    // all remaining JSON-serializable top-level parameters intact.
    const out: Record<string, ESObject> = {};
    let count = 0;
    try {
      const keys = Object.keys(parameters);
      for (let i = 0; i < keys.length; i++) {
        const key = keys[i];
        try {
          const json = JSON.stringify((parameters as Record<string, ESObject>)[key], jsonReplacer);
          if (json !== undefined) {
            out[key] = JSON.parse(json);
            count++;
          }
        } catch {
          // skip unserializable entry
        }
      }
    } catch {
      return undefined;
    }
    return count > 0 ? out : undefined;
  }
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
  const params = serializeParameters(want.parameters);
  if (params !== undefined) {
    out["parameters"] = params;
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
  try {
    getAbilityWantNative().nshAbilityWant(kind, payload);
  } catch (error) {
    console.warn(`NSH ability want delivery failed: ${String(error)}`);
  }
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
