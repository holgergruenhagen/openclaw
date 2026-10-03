const DEFAULT_TELEGRAM_API_ROOT = "https://api.telegram.org";

const TELEGRAM_BOT_ENDPOINT_SEGMENT_RE = /^bot\d+:[^/]+$/u;

function isTelegramBotEndpointSegment(segment: string): boolean {
  try {
    return TELEGRAM_BOT_ENDPOINT_SEGMENT_RE.test(decodeURIComponent(segment));
  } catch {
    return TELEGRAM_BOT_ENDPOINT_SEGMENT_RE.test(segment);
  }
}

export function normalizeTelegramApiRoot(apiRoot?: string): string {
  const trimmed = apiRoot?.trim();
  if (!trimmed) {
    return DEFAULT_TELEGRAM_API_ROOT;
  }

  let normalized = trimmed.replace(/\/+$/u, "");
  const url = URL.parse(normalized);
  if (url) {
    const segments = url.pathname.split("/").filter(Boolean);
    if (segments.length > 0 && isTelegramBotEndpointSegment(segments[segments.length - 1] ?? "")) {
      segments.pop();
      url.pathname = segments.length > 0 ? `/${segments.join("/")}` : "/";
      url.search = "";
      url.hash = "";
      normalized = url.toString().replace(/\/+$/u, "");
    }
  }
  return normalized;
}

export function hasTelegramBotEndpointApiRoot(apiRoot: unknown): boolean {
  if (typeof apiRoot !== "string" || !apiRoot.trim()) {
    return false;
  }
  const segments = URL.parse(apiRoot.trim())?.pathname.split("/").filter(Boolean);
  const last = segments?.at(-1);
  return Boolean(last && isTelegramBotEndpointSegment(last));
}

function readRequestUrl(input: unknown): string | null {
  if (typeof input === "string") {
    return input;
  }
  if (input instanceof URL) {
    return input.toString();
  }
  if (input instanceof Request) {
    return input.url;
  }
  return null;
}

export function extractTelegramApiMethod(input: unknown): string | null {
  const url = readRequestUrl(input);
  const segments = URL.parse(url ?? "")
    ?.pathname.split("/")
    .filter(Boolean);
  return segments?.at(-1)?.toLowerCase() ?? null;
}
