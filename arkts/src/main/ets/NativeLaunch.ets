import { AbilityLifecycleCallback, common, UIAbility, Want } from "@kit.AbilityKit";
import { fileIo as fs } from "@kit.CoreFileKit";

const MAX_PENDING_LAUNCH_URLS = 64;
const MAX_OPEN_FILES_PER_WANT = 16;
const MAX_STAGED_FILE_BYTES = 256 * 1024 * 1024;
const MAX_STAGED_BATCH_BYTES = 512 * 1024 * 1024;
const MAX_CACHE_TOTAL_BYTES = 1024 * 1024 * 1024;
const STAGED_CACHE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const STAGED_SUBDIR = "nsh-opened-files";
const COPY_CHUNK_BYTES = 64 * 1024;

interface DeclaredFileType {
  extensions: string[];
  mimeTypes: string[];
}

interface PinnedFileSource {
  fd: number;
  name: string;
}

class AbilityLaunchQueue {
  pending: string[] = [];
  receiver?: (url: string) => void;
  work: Promise<void> = Promise.resolve();
  declaredTypes?: DeclaredFileType[];

  deliver(url: string): void {
    if (this.receiver !== undefined) {
      this.receiver(url);
      return;
    }
    if (this.pending.length >= MAX_PENDING_LAUNCH_URLS) {
      this.pending.shift();
    }
    this.pending.push(url);
  }
}

const queues: Map<common.UIAbilityContext, AbilityLaunchQueue> = new Map();
let lifecycleRegistered = false;
let stageSequence = 0;

class LaunchLifecycleCleanup extends AbilityLifecycleCallback {
  onAbilityCreate(_ability: UIAbility): void {}
  onWindowStageCreate(_ability: UIAbility): void {}
  onWindowStageActive(_ability: UIAbility): void {}
  onWindowStageInactive(_ability: UIAbility): void {}
  onWindowStageDestroy(_ability: UIAbility): void {}
  onAbilityDestroy(ability: UIAbility): void {
    queues.delete(ability.context);
  }
  onAbilityForeground(_ability: UIAbility): void {}
  onAbilityBackground(_ability: UIAbility): void {}
  onAbilityContinue(_ability: UIAbility): void {}
}

function ensureQueue(context: common.UIAbilityContext): AbilityLaunchQueue {
  if (!lifecycleRegistered) {
    lifecycleRegistered = true;
    try {
      context.getApplicationContext().on("abilityLifecycle", new LaunchLifecycleCleanup());
    } catch {}
  }
  let queue = queues.get(context);
  if (queue === undefined) {
    queue = new AbilityLaunchQueue();
    queues.set(context, queue);
  }
  return queue;
}

function decodeUtf8(bytes: Uint8Array): string {
  let out = "";
  let i = 0;
  while (i < bytes.length) {
    const b0 = bytes[i++];
    if (b0 < 0x80) {
      out += String.fromCharCode(b0);
    } else if ((b0 & 0xe0) === 0xc0 && i < bytes.length) {
      const b1 = bytes[i++];
      out += String.fromCharCode(((b0 & 0x1f) << 6) | (b1 & 0x3f));
    } else if ((b0 & 0xf0) === 0xe0 && i + 1 < bytes.length) {
      const b1 = bytes[i++];
      const b2 = bytes[i++];
      out += String.fromCharCode(((b0 & 0x0f) << 12) | ((b1 & 0x3f) << 6) | (b2 & 0x3f));
    } else if ((b0 & 0xf8) === 0xf0 && i + 2 < bytes.length) {
      const b1 = bytes[i++];
      const b2 = bytes[i++];
      const b3 = bytes[i++];
      const cp =
        (((b0 & 0x07) << 18) | ((b1 & 0x3f) << 12) | ((b2 & 0x3f) << 6) | (b3 & 0x3f)) - 0x10000;
      out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
    }
  }
  return out;
}

function loadDeclaredFileTypes(
  context: common.UIAbilityContext,
  queue: AbilityLaunchQueue,
): DeclaredFileType[] {
  if (queue.declaredTypes !== undefined) {
    return queue.declaredTypes;
  }
  const parsed: DeclaredFileType[] = [];
  try {
    const raw = context.resourceManager.getRawFileContentSync("app.json");
    const doc = JSON.parse(decodeUtf8(raw)) as Record<string, Object>;
    const list = doc["fileTypes"];
    if (Array.isArray(list)) {
      for (let i = 0; i < list.length; i++) {
        const entry = list[i] as Record<string, Object>;
        if (!entry || typeof entry !== "object") {
          continue;
        }
        const exts: string[] = [];
        const rawExts = entry["extensions"];
        if (Array.isArray(rawExts)) {
          for (let j = 0; j < rawExts.length; j++) {
            const v = rawExts[j];
            if (typeof v === "string" && v.trim().length > 0) {
              const cleaned = v.trim().toLowerCase().replace(/^\.+/, "");
              if (cleaned.length > 0) {
                exts.push(cleaned);
              }
            }
          }
        }
        const mimes: string[] = [];
        const rawMimes = entry["mimeTypes"];
        if (Array.isArray(rawMimes)) {
          for (let j = 0; j < rawMimes.length; j++) {
            const v = rawMimes[j];
            if (typeof v === "string" && v.trim().length > 0) {
              mimes.push(v.trim().toLowerCase());
            }
          }
        }
        if (exts.length > 0 || mimes.length > 0) {
          parsed.push({ extensions: exts, mimeTypes: mimes });
        }
      }
    }
  } catch {}
  queue.declaredTypes = parsed;
  return parsed;
}

function decodeUriPathSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function basenameFromUri(uri: string): string {
  const withoutQuery = uri.split(/[?#]/)[0];
  const slash = withoutQuery.lastIndexOf("/");
  const raw = slash >= 0 ? withoutQuery.slice(slash + 1) : withoutQuery;
  const decoded = decodeUriPathSegment(raw).trim();
  return sanitizeFilename(decoded);
}

function sanitizeFilename(name: string): string {
  const cleaned = name
    .replace(/[\x00-\x1f\x7f/\\]/g, "_")
    .replace(/^\.+$/, "")
    .trim();
  if (cleaned.length === 0) {
    return "file";
  }
  return cleaned.length > 160 ? cleaned.slice(cleaned.length - 160) : cleaned;
}

function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot >= name.length - 1) {
    return "";
  }
  return name.slice(dot + 1).toLowerCase();
}

function mimeMatches(declared: string, actual: string): boolean {
  if (declared === "*/*" || declared === actual) {
    return true;
  }
  if (declared.endsWith("/*")) {
    const prefix = declared.slice(0, declared.length - 1);
    return actual.startsWith(prefix);
  }
  return false;
}

function matchesDeclaredFileTypes(
  types: DeclaredFileType[],
  filename: string,
  wantMime?: string,
): boolean {
  if (types.length === 0) {
    return false;
  }
  const ext = extensionOf(filename);
  const mime = wantMime?.trim().toLowerCase() ?? "";
  for (let i = 0; i < types.length; i++) {
    const entry = types[i];
    if (ext.length > 0 && entry.extensions.includes(ext)) {
      return true;
    }
    if (mime.length > 0) {
      for (let j = 0; j < entry.mimeTypes.length; j++) {
        if (mimeMatches(entry.mimeTypes[j], mime)) {
          return true;
        }
      }
    }
  }
  return false;
}

function isOwnSandboxUri(context: common.UIAbilityContext, uri: string): boolean {
  const withoutQuery = uri.split(/[?#]/)[0];
  let path = withoutQuery;
  if (path.toLowerCase().startsWith("file://")) {
    const rest = path.slice(7);
    const slash = rest.indexOf("/");
    if (slash < 0) {
      return false;
    }
    const host = rest.slice(0, slash);
    path = decodeUriPathSegment(rest.slice(slash));
    const bundleName = context.abilityInfo?.bundleName ?? "";
    if (bundleName.length > 0 && host === bundleName) {
      return true;
    }
  }
  const normalized = path.replace(/\/+/g, "/");
  const roots: string[] = [
    context.filesDir,
    context.cacheDir,
    context.tempDir,
    context.databaseDir,
    context.preferencesDir,
    context.bundleCodeDir,
  ].filter((dir: string | undefined): boolean => typeof dir === "string" && dir.length > 0);
  for (let i = 0; i < roots.length; i++) {
    const root = roots[i].replace(/\/+$/, "");
    if (normalized === root || normalized.startsWith(`${root}/`)) {
      return true;
    }
  }
  return false;
}

function pathToFileUrl(path: string): string {
  const parts = path.split("/");
  const encoded: string[] = [];
  for (let i = 0; i < parts.length; i++) {
    if (i === 0 && parts[i] === "") {
      continue;
    }
    encoded.push(encodeURIComponent(parts[i]));
  }
  return `file:///${encoded.join("/")}`;
}

function collectWantUris(want: Want): string[] {
  const uris: string[] = [];
  const seen: Set<string> = new Set();
  const stream = want.parameters?.["ability.params.stream"];
  if (Array.isArray(stream)) {
    for (let i = 0; i < stream.length; i++) {
      const item = stream[i];
      if (typeof item === "string" && item.length > 0 && !seen.has(item)) {
        seen.add(item);
        uris.push(item);
      }
    }
  }
  if (typeof want.uri === "string" && want.uri.length > 0 && !seen.has(want.uri)) {
    seen.add(want.uri);
    uris.push(want.uri);
  }
  return uris;
}

function pruneStagedCache(rootDir: string): void {
  try {
    if (!fs.accessSync(rootDir)) {
      return;
    }
    const now = Date.now();
    const entries = fs.listFileSync(rootDir);
    interface CacheDirInfo {
      path: string;
      mtimeMs: number;
      bytes: number;
    }
    const live: CacheDirInfo[] = [];
    let totalBytes = 0;
    for (let i = 0; i < entries.length; i++) {
      const dirPath = `${rootDir}/${entries[i]}`;
      try {
        const st = fs.statSync(dirPath);
        const mtimeMs = st.mtime * 1000;
        let dirBytes = st.size;
        if (st.isDirectory()) {
          dirBytes = 0;
          const files = fs.listFileSync(dirPath);
          for (let j = 0; j < files.length; j++) {
            try {
              dirBytes += fs.statSync(`${dirPath}/${files[j]}`).size;
            } catch {}
          }
        }
        if (now - mtimeMs > STAGED_CACHE_TTL_MS) {
          try {
            fs.rmdirSync(dirPath);
          } catch {
            try {
              fs.unlinkSync(dirPath);
            } catch {}
          }
          continue;
        }
        totalBytes += dirBytes;
        live.push({ path: dirPath, mtimeMs, bytes: dirBytes });
      } catch {}
    }
    if (totalBytes > MAX_CACHE_TOTAL_BYTES) {
      live.sort((a: CacheDirInfo, b: CacheDirInfo): number => a.mtimeMs - b.mtimeMs);
      for (let i = 0; i < live.length && totalBytes > MAX_CACHE_TOTAL_BYTES; i++) {
        try {
          fs.rmdirSync(live[i].path);
        } catch {
          try {
            fs.unlinkSync(live[i].path);
          } catch {}
        }
        totalBytes -= live[i].bytes;
      }
    }
  } catch {}
}

async function stagePinnedFile(
  rootDir: string,
  source: PinnedFileSource,
  remainingBatchBytes: number,
): Promise<{ url?: string; bytes: number }> {
  const maxBytes = Math.min(MAX_STAGED_FILE_BYTES, remainingBatchBytes);
  const subDir = `${rootDir}/${Date.now()}-${++stageSequence}`;
  const destPath = `${subDir}/${source.name}`;
  let destFile: fs.File | undefined = undefined;
  let written = 0;
  let ok = false;
  try {
    try {
      fs.mkdirSync(rootDir, true);
    } catch {}
    fs.mkdirSync(subDir, true);
    destFile = fs.openSync(
      destPath,
      fs.OpenMode.CREATE | fs.OpenMode.WRITE_ONLY | fs.OpenMode.TRUNC,
    );
    const buffer = new ArrayBuffer(COPY_CHUNK_BYTES);
    while (true) {
      const read = await fs.read(source.fd, buffer);
      if (read <= 0) {
        break;
      }
      if (written + read > maxBytes) {
        return { bytes: 0 };
      }
      await fs.write(destFile.fd, buffer, { length: read });
      written += read;
    }
    ok = true;
    return { url: pathToFileUrl(destPath), bytes: written };
  } catch {
    return { bytes: 0 };
  } finally {
    if (destFile !== undefined) {
      try {
        fs.closeSync(destFile.fd);
      } catch {}
    }
    try {
      fs.closeSync(source.fd);
    } catch {}
    if (!ok) {
      try {
        fs.unlinkSync(destPath);
      } catch {}
      try {
        fs.rmdirSync(subDir);
      } catch {}
    }
  }
}

/**
 * Accepts cold (`onCreate`) and warm (`onNewWant`) launch Wants for an Ability,
 * preserving arrival order in a per-Ability queue until `bindLaunchSurface` attaches,
 * and staging declared `file:` Wants into app-local cache `file:///...` URLs.
 */
export function onLaunchWant(
  context: common.UIAbilityContext,
  want: Want,
  _isCreate: boolean,
): boolean {
  const uris = collectWantUris(want);
  if (uris.length === 0) {
    return false;
  }
  const queue = ensureQueue(context);
  const regularUrls: string[] = [];
  const fileUris: string[] = [];
  for (let i = 0; i < uris.length; i++) {
    const uri = uris[i];
    if (uri.toLowerCase().startsWith("file:")) {
      fileUris.push(uri);
    } else {
      regularUrls.push(uri);
    }
  }

  let acceptedAny = false;
  if (regularUrls.length > 0) {
    acceptedAny = true;
    queue.work = queue.work
      .then((): void => {
        for (let i = 0; i < regularUrls.length; i++) {
          queue.deliver(regularUrls[i]);
        }
      })
      .catch((): void => {});
  }

  if (fileUris.length > 0) {
    const declared = loadDeclaredFileTypes(context, queue);
    const wantMime = typeof want.type === "string" ? want.type : undefined;
    const pinned: PinnedFileSource[] = [];
    for (let i = 0; i < fileUris.length && pinned.length < MAX_OPEN_FILES_PER_WANT; i++) {
      const uri = fileUris[i];
      if (isOwnSandboxUri(context, uri)) {
        continue;
      }
      const name = basenameFromUri(uri);
      if (!matchesDeclaredFileTypes(declared, name, wantMime)) {
        continue;
      }
      try {
        // Pin HarmonyOS's temporary URI read grant synchronously before onCreate/onNewWant returns.
        const file = fs.openSync(uri, fs.OpenMode.READ_ONLY);
        pinned.push({ fd: file.fd, name });
      } catch {}
    }
    if (pinned.length > 0) {
      acceptedAny = true;
      const cacheRoot = `${context.cacheDir}/${STAGED_SUBDIR}`;
      queue.work = queue.work
        .then(async (): Promise<void> => {
          pruneStagedCache(cacheRoot);
          let remainingBatch = MAX_STAGED_BATCH_BYTES;
          for (let i = 0; i < pinned.length; i++) {
            const staged = await stagePinnedFile(cacheRoot, pinned[i], remainingBatch);
            if (staged.url !== undefined) {
              remainingBatch = Math.max(0, remainingBatch - staged.bytes);
              queue.deliver(staged.url);
            }
          }
        })
        .catch((): void => {});
    }
  }

  return acceptedAny;
}

/**
 * Subscribes a live `NshSurface` to an Ability's launch queue, immediately draining
 * any buffered cold/warm launch URLs and staged files in arrival order.
 */
export function bindLaunchSurface(
  context: common.UIAbilityContext,
  receiver: (url: string) => void,
): () => void {
  const queue = ensureQueue(context);
  queue.receiver = receiver;
  if (queue.pending.length > 0) {
    const queued = queue.pending.splice(0, queue.pending.length);
    for (let i = 0; i < queued.length; i++) {
      receiver(queued[i]);
    }
  }
  return (): void => {
    if (queue.receiver === receiver) {
      queue.receiver = undefined;
    }
  };
}
