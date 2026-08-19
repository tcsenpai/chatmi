// chatmi sidecar — stateless streaming bridge to MiMo via the mimo-api SDK.
// Reads one JSON request per line on stdin, writes JSON-lines chunks on stdout.
// The API key lives ONLY here (passed via env by the Rust core), never in the webview.
import { MiMo, type Message } from "mimo-api";
import { createInterface } from "node:readline";

type Request = {
  id: string;
  messages: Message[];
  opts?: { model?: string; system?: string; temperature?: number };
};

// The SDK appends "/v1/chat/completions" itself, so baseURL must NOT end in /v1.
// Strip a trailing /v1 defensively so a MIMO_URL like ".../v1" still works.
const rawUrl = process.env.MIMO_URL?.replace(/\/v1\/?$/, "");
const mimo = new MiMo({
  apiKey: process.env.MIMO_API_KEY,
  baseURL: rawUrl || undefined, // undefined → SDK default
});

function emit(obj: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

// Multimodal parts require the mimo-v2.5 model; plain text uses the reasoning
// default (mimo-v2.5-pro). Auto-pick so the frontend never has to.
function hasMultimodal(messages: Message[]): boolean {
  return messages.some(
    (m) =>
      Array.isArray(m.content) &&
      m.content.some((p) => p.type !== "text"),
  );
}

async function handle(req: Request): Promise<void> {
  try {
    const model =
      req.opts?.model ?? (hasMultimodal(req.messages) ? "mimo-v2.5" : undefined);
    const stream = mimo.chatStreamWithReasoning(req.messages, {
      model: model as never,
      system: req.opts?.system,
      temperature: req.opts?.temperature,
    });
    for await (const part of stream) {
      emit({ id: req.id, type: part.type, text: part.text });
    }
    emit({ id: req.id, type: "done" });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    emit({ id: req.id, type: "error", message });
  }
}

const rl = createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let req: Request;
  try {
    req = JSON.parse(trimmed);
  } catch {
    emit({ id: "?", type: "error", message: "bad json on stdin" });
    return;
  }
  // ponytail: fire-and-forget; each request streams independently by id.
  void handle(req);
});
