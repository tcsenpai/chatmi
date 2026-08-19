<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { marked } from "marked";

  type Msg = {
    id: string;
    role: "user" | "assistant";
    content: string;
    reasoning: string;
  };
  type Conversation = { id: string; title: string; created_at: number };

  let conversations = $state<Conversation[]>([]);
  let activeId = $state<string | null>(null);
  let dbMessages = $state<Msg[]>([]); // rows loaded from SQLite for activeId
  let input = $state("");
  let errorMsg = $state<string | null>(null);
  let showReasoning = $state<Record<string, boolean>>({});
  let messagesEl = $state<HTMLElement | null>(null);
  let composerEl = $state<HTMLTextAreaElement | null>(null);

  // Live streaming buffers keyed by assistant id. These survive conversation
  // switches, so an in-flight reply (not yet in the DB) reappears when you come
  // back to its conversation. `pendingUser` holds the optimistic user bubble.
  type Stream = { convId: string; content: string; reasoning: string; pendingUser?: Msg };
  let streams = $state<Record<string, Stream>>({});

  // The visible message list: DB rows for the active conversation, plus any
  // in-flight user/assistant messages belonging to it, merged by id.
  const messages = $derived.by(() => {
    const out = [...dbMessages];
    const byId = new Set(out.map((m) => m.id));
    for (const [id, s] of Object.entries(streams)) {
      if (s.convId !== activeId) continue;
      if (s.pendingUser && !byId.has(s.pendingUser.id)) out.push(s.pendingUser);
      if (!byId.has(id))
        out.push({ id, role: "assistant", content: s.content, reasoning: s.reasoning });
    }
    return out;
  });

  // Is a reply still generating for the conversation currently on screen?
  const streamingHere = $derived(
    Object.values(streams).some((s) => s.convId === activeId),
  );

  type Attachment = { kind: "image" | "audio"; mime: string; data: string; name: string };
  let attachments = $state<Attachment[]>([]);

  // A stored user message may be a JSON content-array (multimodal). Parse it into
  // { text, parts } for rendering; plain strings pass through as text.
  function parseContent(raw: string): { text: string; parts: Attachment[] } {
    if (!raw.startsWith("[")) return { text: raw, parts: [] };
    try {
      const arr = JSON.parse(raw);
      if (!Array.isArray(arr)) return { text: raw, parts: [] };
      let text = "";
      const parts: Attachment[] = [];
      for (const p of arr) {
        if (p.type === "text") text = p.text;
        else if (p.type === "image_url")
          parts.push({ kind: "image", mime: "", data: p.image_url.url, name: "image" });
        else if (p.type === "input_audio")
          parts.push({ kind: "audio", mime: "", data: p.input_audio.data, name: "audio" });
      }
      return { text, parts };
    } catch {
      return { text: raw, parts: [] };
    }
  }

  function imageSrc(a: Attachment): string {
    // stored parts already carry a data: URI; fresh attachments carry raw base64
    return a.data.startsWith("data:") ? a.data : `data:${a.mime};base64,${a.data}`;
  }

  // Autoscroll to bottom whenever messages change, unless the user scrolled up.
  function scrollToBottom() {
    if (!messagesEl) return;
    const nearBottom =
      messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 120;
    if (nearBottom) messagesEl.scrollTop = messagesEl.scrollHeight;
  }
  $effect(() => {
    // depend on messages so this re-runs on every stream update
    messages;
    requestAnimationFrame(scrollToBottom);
  });

  function autoGrow() {
    if (!composerEl) return;
    composerEl.style.height = "auto";
    composerEl.style.height = Math.min(composerEl.scrollHeight, 200) + "px";
  }

  async function loadConversations() {
    conversations = await invoke("list_conversations");
  }

  async function openConversation(id: string) {
    activeId = id;
    errorMsg = null;
    dbMessages = await invoke("get_messages", { conversationId: id });
  }

  async function newChat() {
    const conv: Conversation = await invoke("new_conversation");
    conversations = [conv, ...conversations];
    await openConversation(conv.id);
  }

  async function removeChat(id: string, e: Event) {
    e.stopPropagation();
    await invoke("delete_conversation", { id });
    conversations = conversations.filter((c) => c.id !== id);
    if (activeId === id) {
      activeId = null;
      dbMessages = [];
    }
  }

  async function addFile() {
    const picked = await invoke<Attachment | null>("pick_attachment");
    if (picked) attachments = [...attachments, picked];
  }

  function removeAttachment(i: number) {
    attachments = attachments.filter((_, idx) => idx !== i);
  }

  async function onPaste(e: ClipboardEvent) {
    const items = e.clipboardData?.items;
    if (!items) return;
    for (const item of items) {
      if (!item.type.startsWith("image/")) continue;
      const file = item.getAsFile();
      if (!file) continue;
      e.preventDefault();
      const buf = new Uint8Array(await file.arrayBuffer());
      let bin = "";
      for (const b of buf) bin += String.fromCharCode(b);
      attachments = [
        ...attachments,
        { kind: "image", mime: file.type, data: btoa(bin), name: file.name || "pasted" },
      ];
    }
  }

  async function send() {
    const text = input.trim();
    if ((!text && attachments.length === 0) || streamingHere) return;
    if (!activeId) await newChat();
    const convId = activeId!;
    const sent = attachments;
    attachments = [];
    input = "";
    autoGrow();
    errorMsg = null;

    // optimistic user bubble — encode attachments the same way Rust stores them
    const optimistic =
      sent.length === 0
        ? text
        : JSON.stringify([
            ...(text ? [{ type: "text", text }] : []),
            ...sent.map((a) =>
              a.kind === "image"
                ? { type: "image_url", image_url: { url: imageSrc(a) } }
                : { type: "input_audio", input_audio: { data: a.data } },
            ),
          ]);
    const pendingUser: Msg = {
      id: crypto.randomUUID(),
      role: "user",
      content: optimistic,
      reasoning: "",
    };

    try {
      const assistantId: string = await invoke("send_message", {
        conversationId: convId,
        content: text,
        attachments: sent.map(({ kind, mime, data }) => ({ kind, mime, data })),
      });
      // Register the live stream buffer; keyed by the real assistant id.
      streams = {
        ...streams,
        [assistantId]: { convId, content: "", reasoning: "", pendingUser },
      };
    } catch (err) {
      errorMsg = String(err);
    }

    loadConversations(); // refresh titles/order
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  onMount(async () => {
    await loadConversations();

    await listen<{ id: string; kind: string; text: string }>("mimo-chunk", (e) => {
      const { id, kind, text } = e.payload;
      const s = streams[id];
      if (!s) return; // stream we don't know about (e.g. after reload) — ignore
      streams[id] = {
        ...s,
        reasoning: kind === "reasoning" ? s.reasoning + text : s.reasoning,
        content: kind === "reasoning" ? s.content : s.content + text,
      };
    });

    await listen<{ id: string }>("mimo-done", async (e) => {
      const s = streams[e.payload.id];
      // Persisted now — pull the DB rows if this conversation is on screen,
      // then drop the live buffer.
      if (s && s.convId === activeId) {
        dbMessages = await invoke("get_messages", { conversationId: s.convId });
      }
      const { [e.payload.id]: _drop, ...rest } = streams;
      streams = rest;
    });

    await listen<{ id: string; message: string }>("mimo-error", (e) => {
      const { [e.payload.id]: _drop, ...rest } = streams;
      streams = rest;
      errorMsg = e.payload.message;
    });
  });

  function render(md: string): string {
    return marked.parse(md, { async: false }) as string;
  }
</script>

<div class="layout">
  <aside class="sidebar">
    <button class="new-btn" onclick={newChat}><span class="plus">+</span> New chat</button>
    <div class="conv-list">
      {#each conversations as conv (conv.id)}
        <div
          class="conv"
          class:active={conv.id === activeId}
          role="button"
          tabindex="0"
          onclick={() => openConversation(conv.id)}
          onkeydown={(e) => e.key === "Enter" && openConversation(conv.id)}
        >
          <span class="conv-title">{conv.title}</span>
          <button class="del" onclick={(e) => removeChat(conv.id, e)} aria-label="delete">×</button>
        </div>
      {/each}
    </div>
  </aside>

  <main class="chat">
    <div class="messages" bind:this={messagesEl}>
      {#if messages.length === 0}
        <div class="empty">
          <div class="empty-mark">✦</div>
          <p>Ask MiMo anything.</p>
        </div>
      {/if}
      {#each messages as m (m.id)}
        <div class="msg {m.role}">
          {#if m.reasoning}
            <button
              class="reasoning-toggle"
              aria-expanded={showReasoning[m.id] ?? false}
              onclick={() => (showReasoning[m.id] = !showReasoning[m.id])}
            >
              <span class="caret" class:open={showReasoning[m.id]}>▸</span> thinking
            </button>
            {#if showReasoning[m.id]}
              <div class="reasoning">{m.reasoning}</div>
            {/if}
          {/if}
          {#if m.role === "assistant"}
            {#if m.content}
              <div class="bubble">{@html render(m.content)}</div>
            {:else if streams[m.id]}
              <div class="bubble typing"><span></span><span></span><span></span></div>
            {/if}
          {:else}
            {@const parsed = parseContent(m.content)}
            <div class="bubble">
              {#if parsed.parts.length}
                <div class="attach-row">
                  {#each parsed.parts as p, i (i)}
                    {#if p.kind === "image"}
                      <img class="attach-thumb" src={imageSrc(p)} alt="attachment" />
                    {:else}
                      <div class="attach-chip">🎵 audio</div>
                    {/if}
                  {/each}
                </div>
              {/if}
              {#if parsed.text}<div class="bubble-text">{parsed.text}</div>{/if}
            </div>
          {/if}
        </div>
      {/each}
      {#if errorMsg}
        <div class="error" role="alert">⚠ {errorMsg}</div>
      {/if}
    </div>

    <div class="composer">
      <div class="composer-box">
        {#if attachments.length}
          <div class="pending-attachments">
            {#each attachments as a, i (i)}
              <div class="pending">
                {#if a.kind === "image"}
                  <img src={imageSrc(a)} alt={a.name} />
                {:else}
                  <div class="pending-audio">🎵</div>
                {/if}
                <button class="pending-remove" onclick={() => removeAttachment(i)} aria-label="remove attachment">×</button>
              </div>
            {/each}
          </div>
        {/if}
        <div class="composer-inner">
          <button class="attach-btn" onclick={addFile} aria-label="Attach file" title="Attach image or audio">📎</button>
          <textarea
            bind:this={composerEl}
            bind:value={input}
            oninput={autoGrow}
            onkeydown={onKey}
            onpaste={onPaste}
            placeholder="Message MiMo…"
            rows="1"
            aria-label="Message input"
          ></textarea>
          <button
            class="send"
            onclick={send}
            disabled={streamingHere || (!input.trim() && attachments.length === 0)}
            aria-label="Send message"
          >
            {#if streamingHere}
              <span class="spinner"></span>
            {:else}
              ↑
            {/if}
          </button>
        </div>
      </div>
    </div>
  </main>
</div>

<style>
  .layout { display: flex; height: 100vh; }

  /* ---------- sidebar ---------- */
  .sidebar {
    width: 260px;
    background: var(--color-surface);
    border-right: 1px solid var(--color-border-subtle);
    display: flex;
    flex-direction: column;
    padding: var(--space-3);
    gap: var(--space-2);
  }
  .new-btn {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: transparent;
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    padding: var(--space-3);
    font-weight: 500;
    transition: background var(--transition), border-color var(--transition);
  }
  .new-btn:hover { background: var(--color-surface-hover); border-color: var(--color-primary); }
  .plus { color: var(--color-primary); font-weight: 600; }

  .conv-list { overflow-y: auto; display: flex; flex-direction: column; gap: 2px; margin-top: var(--space-2); }
  .conv {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    color: var(--color-text-secondary);
    transition: background var(--transition), color var(--transition);
  }
  .conv:hover { background: var(--color-surface-hover); color: var(--color-text); }
  .conv.active { background: var(--color-surface-active); color: var(--color-text); }
  .conv-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.9rem; }
  .del {
    background: none; border: none; color: var(--color-text-tertiary);
    font-size: 1.15rem; line-height: 1; padding: 0 var(--space-1);
    opacity: 0; transition: opacity var(--transition), color var(--transition);
  }
  .conv:hover .del { opacity: 1; }
  .del:hover { color: var(--color-danger); }

  /* ---------- chat ---------- */
  .chat { flex: 1; display: flex; flex-direction: column; min-width: 0; }
  .messages {
    flex: 1; overflow-y: auto; padding: var(--space-6);
    display: flex; flex-direction: column; gap: var(--space-6);
    scroll-behavior: smooth;
  }
  .empty { margin: auto; text-align: center; color: var(--color-text-tertiary); }
  .empty-mark { font-size: 2rem; color: var(--color-primary); margin-bottom: var(--space-2); }

  .msg { display: flex; flex-direction: column; gap: var(--space-2); max-width: 760px; width: 100%; align-self: center; }
  .msg.user { align-items: flex-end; }
  .bubble { padding: var(--space-3) var(--space-4); border-radius: var(--radius-lg); }
  .msg.user .bubble {
    background: var(--color-user-bubble);
    max-width: 80%;
    white-space: pre-wrap;
    border-bottom-right-radius: var(--radius-sm);
  }
  .msg.assistant .bubble { background: transparent; padding-left: 0; padding-right: 0; }

  /* typing indicator */
  .typing { display: inline-flex; gap: 5px; align-items: center; padding: var(--space-3) 0; }
  .typing span {
    width: 7px; height: 7px; border-radius: 50%;
    background: var(--color-text-tertiary);
    animation: bounce 1.2s infinite ease-in-out;
  }
  .typing span:nth-child(2) { animation-delay: 0.15s; }
  .typing span:nth-child(3) { animation-delay: 0.3s; }
  @keyframes bounce { 0%, 60%, 100% { opacity: 0.3; transform: translateY(0); } 30% { opacity: 1; transform: translateY(-4px); } }

  /* reasoning / thinking */
  .reasoning-toggle {
    display: inline-flex; align-items: center; gap: var(--space-2);
    background: none; border: none; color: var(--color-text-tertiary);
    font-size: 0.82rem; padding: 0; text-align: left;
    align-self: flex-start; transition: color var(--transition);
  }
  .reasoning-toggle:hover { color: var(--color-text-secondary); }
  .caret { display: inline-block; transition: transform var(--transition); font-size: 0.7rem; }
  .caret.open { transform: rotate(90deg); }
  .reasoning {
    background: var(--color-reasoning-bg);
    border-left: 2px solid var(--color-border);
    padding: var(--space-3);
    border-radius: var(--radius-sm);
    font-size: 0.85rem;
    color: var(--color-text-secondary);
    white-space: pre-wrap;
  }

  .error {
    color: var(--color-danger); align-self: center;
    background: color-mix(in srgb, var(--color-danger) 12%, transparent);
    padding: var(--space-2) var(--space-4); border-radius: var(--radius-md);
    font-size: 0.9rem;
  }

  /* ---------- composer ---------- */
  .composer { padding: var(--space-4) var(--space-6) var(--space-6); }
  .composer-inner {
    display: flex; gap: var(--space-2); align-items: flex-end;
    max-width: 760px; margin: 0 auto;
    background: var(--color-input);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    padding: var(--space-2) var(--space-2) var(--space-2) var(--space-4);
    transition: border-color var(--transition);
  }
  .composer-inner:focus-within { border-color: var(--color-primary); }
  textarea {
    flex: 1; background: transparent; color: var(--color-text);
    border: none; padding: var(--space-2) 0; resize: none; font: inherit;
    max-height: 200px; line-height: 1.5;
  }
  textarea:focus { outline: none; }
  textarea::placeholder { color: var(--color-text-tertiary); }
  .send {
    flex-shrink: 0;
    width: 40px; height: 40px; display: flex; align-items: center; justify-content: center;
    background: var(--color-primary); color: var(--color-primary-text);
    border: none; border-radius: var(--radius-md);
    font-size: 1.1rem; font-weight: 600;
    transition: background var(--transition), opacity var(--transition);
  }
  .send:hover:not(:disabled) { background: var(--color-primary-hover); }
  .send:disabled { opacity: 0.35; cursor: default; }
  .spinner {
    width: 14px; height: 14px; border-radius: 50%;
    border: 2px solid color-mix(in srgb, var(--color-primary-text) 40%, transparent);
    border-top-color: var(--color-primary-text);
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }

  /* ---------- attachments ---------- */
  .attach-row { display: flex; flex-wrap: wrap; gap: var(--space-2); margin-bottom: var(--space-2); }
  .attach-thumb {
    max-width: 220px; max-height: 220px; border-radius: var(--radius-md);
    border: 1px solid var(--color-border); object-fit: cover;
  }
  .attach-chip {
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-md); padding: var(--space-2) var(--space-3); font-size: 0.85rem;
  }
  .bubble-text { white-space: pre-wrap; }

  .composer-box {
    max-width: 760px; margin: 0 auto;
    background: var(--color-input);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    transition: border-color var(--transition);
  }
  .composer-box:focus-within { border-color: var(--color-primary); }
  /* the inner row no longer draws its own border once wrapped */
  .composer-box .composer-inner {
    background: transparent; border: none; max-width: none; margin: 0;
  }

  .pending-attachments {
    display: flex; flex-wrap: wrap; gap: var(--space-2);
    padding: var(--space-3) var(--space-3) 0;
  }
  .pending { position: relative; }
  .pending img {
    width: 56px; height: 56px; object-fit: cover;
    border-radius: var(--radius-sm); border: 1px solid var(--color-border);
  }
  .pending-audio {
    width: 56px; height: 56px; display: flex; align-items: center; justify-content: center;
    background: var(--color-surface); border-radius: var(--radius-sm); border: 1px solid var(--color-border);
  }
  .pending-remove {
    position: absolute; top: -7px; right: -7px;
    width: 20px; height: 20px; border-radius: 50%; border: none;
    background: var(--color-danger); color: #fff; font-size: 0.85rem; line-height: 1;
    display: flex; align-items: center; justify-content: center;
  }

  .attach-btn {
    flex-shrink: 0; width: 40px; height: 40px;
    display: flex; align-items: center; justify-content: center;
    background: transparent; border: none; border-radius: var(--radius-md);
    font-size: 1.1rem; opacity: 0.7; transition: opacity var(--transition), background var(--transition);
  }
  .attach-btn:hover { opacity: 1; background: var(--color-surface-hover); }
</style>
