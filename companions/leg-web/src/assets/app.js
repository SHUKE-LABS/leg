const tokenKey = "leg-web-launch-token";
const sessionKey = "leg-web-current-session";
const tabKey = "leg-web-tab-id";
const workspaceKey = "leg-web-last-workspace";
const draftPrefix = "leg-web-draft:";
const pendingPrefix = "leg-web-pending:";

const bootstrap = window.location.hash.slice(1);
if (/^[0-9a-f]{64}$/i.test(bootstrap)) {
  window.sessionStorage.setItem(tokenKey, bootstrap);
  window.history.replaceState(null, "", window.location.pathname + window.location.search);
}

const token = window.sessionStorage.getItem(tokenKey);
const tabId = window.sessionStorage.getItem(tabKey) || crypto.randomUUID().replaceAll("-", "");
window.sessionStorage.setItem(tabKey, tabId);

const ui = Object.fromEntries(
  [
    "connection-state",
    "new-conversation",
    "current-session-card",
    "rail-session-name",
    "rail-workspace",
    "welcome",
    "start-form",
    "workspace-input",
    "setup-error",
    "conversation",
    "session-title",
    "workspace-label",
    "provider-model",
    "elapsed-time",
    "turn-status",
    "active-tool",
    "stop-turn",
    "workspace-warning",
    "connection-message",
    "transcript",
    "messages",
    "empty-transcript",
    "new-content",
    "composer",
    "prompt",
    "retry-submission",
    "send",
    "send-error",
    "live-status",
  ].map((id) => [id, document.getElementById(id)]),
);

const state = {
  sessionId: window.sessionStorage.getItem(sessionKey),
  snapshot: null,
  active: null,
  cursor: 0,
  streamAbort: null,
  streamGeneration: 0,
  reconnecting: false,
  pending: null,
  composition: false,
  lastAnnouncedStatus: "",
  connectionText: "",
  submitting: false,
  startBusy: false,
};

let elapsedTimer = null;

class HostError extends Error {
  constructor(code, status) {
    super(code);
    this.code = code;
    this.status = status;
  }
}

function storageKey(prefix, sessionId) {
  return `${prefix}${sessionId}`;
}

function apiErrorText(code) {
  const messages = {
    unauthorized: "This tab is no longer authorized. Open the current launch URL to reconnect.",
    host_shutting_down: "The local host is shutting down. Your draft is kept in this tab.",
    session_busy: "This conversation is already running in another tab. Your message is still here.",
    workspace_required: "Choose an existing workspace folder before sending. The workspace path must be set by creating a new conversation.",
    workspace_must_be_absolute: "Enter an absolute path to an existing folder on the host computer.",
    session_read_only: "This conversation cannot be changed because its saved trail could not be read.",
    submission_id_conflict: "This send ID is already associated with different text. Your draft is preserved; reload the conversation before sending again.",
    submission_id_stale: "This send ID has expired. Your draft is preserved; reload the conversation before sending again.",
    submission_id_unexpected: "Another tab advanced this conversation. Your draft is preserved; reload before sending again.",
    catalog_unavailable: "The host could not start a Leg turn. Check that leg and leg-ui-supervisor are installed, then restart leg-web with --leg-bin and --supervisor-bin if needed. Provider settings come from the environment that starts leg-web.",
    invalid_request: "The host rejected this request. Check the workspace path and try again.",
  };
  return messages[code] || `The local host reported ${code || "an error"}. Your draft is preserved.`;
}

async function api(path, { method = "GET", body } = {}) {
  if (!token) throw new HostError("unauthorized", 401);
  const headers = { Authorization: `Bearer ${token}` };
  const options = { method, headers, cache: "no-store", credentials: "same-origin" };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  let payload = null;
  try {
    payload = await response.json();
  } catch {
    payload = null;
  }
  if (!response.ok) throw new HostError(payload?.error || "host_error", response.status);
  return payload;
}

function setConnection(text, reconnecting = false) {
  state.reconnecting = reconnecting;
  if (state.connectionText !== text) {
    state.connectionText = text;
    ui["connection-state"].textContent = text;
  }
  ui["connection-message"].hidden = !reconnecting;
  ui["connection-message"].textContent = reconnecting
    ? "Reconnecting to the local host. The draft is kept, and no message will be resent."
    : "";
  renderStatus();
}

function showStartError(message) {
  ui["setup-error"].textContent = message;
  ui["setup-error"].hidden = false;
}

function clearStartError() {
  ui["setup-error"].textContent = "";
  ui["setup-error"].hidden = true;
}

function showSendError(message) {
  ui["send-error"].textContent = message;
  ui["send-error"].hidden = !message;
}

function saveDraft() {
  if (!state.sessionId) return;
  window.sessionStorage.setItem(storageKey(draftPrefix, state.sessionId), ui.prompt.value);
}

function savePending() {
  if (!state.sessionId) return;
  if (state.pending) {
    window.sessionStorage.setItem(storageKey(pendingPrefix, state.sessionId), JSON.stringify(state.pending));
  } else {
    window.sessionStorage.removeItem(storageKey(pendingPrefix, state.sessionId));
  }
}

function adjustTextarea() {
  ui.prompt.rows = Math.min(8, Math.max(3, ui.prompt.value.split("\n").length));
}

function absoluteWorkspace(value) {
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value);
}

async function startConversation(event) {
  event.preventDefault();
  if (state.startBusy) return;
  const cwd = ui["workspace-input"].value.trim();
  if (!absoluteWorkspace(cwd)) {
    showStartError("Enter an absolute path, such as /home/name/project or C:\\Users\\name\\project.");
    ui["workspace-input"].focus();
    return;
  }
  state.startBusy = true;
  clearStartError();
  ui["start-form"].querySelector("button[type=submit]").disabled = true;
  try {
    const session = await api("/api/sessions", { method: "POST", body: { cwd } });
    window.sessionStorage.setItem(workspaceKey, cwd);
    await activateSession(session.id);
  } catch (error) {
    showStartError(apiErrorText(error.code || "host_unavailable"));
  } finally {
    state.startBusy = false;
    ui["start-form"].querySelector("button[type=submit]").disabled = false;
  }
}

async function activateSession(sessionId) {
  state.streamGeneration += 1;
  state.streamAbort?.abort();
  state.sessionId = sessionId;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = readPending(sessionId);
  window.sessionStorage.setItem(sessionKey, sessionId);
  ui.prompt.value = window.sessionStorage.getItem(storageKey(draftPrefix, sessionId)) || state.pending?.prompt || "";
  adjustTextarea();
  showSendError("");
  ui.welcome.hidden = true;
  ui.conversation.hidden = false;
  try {
    await api("/api/sessions/select", {
      method: "POST",
      body: { session_id: sessionId, tab_id: tabId },
    });
    await refreshSnapshot();
    setConnection("Connected to local host.");
    connectEvents(state.cursor);
    if (state.pending) await reconcilePending();
  } catch (error) {
    if (error.status === 404) {
      window.sessionStorage.removeItem(sessionKey);
      window.sessionStorage.removeItem(storageKey(pendingPrefix, sessionId));
      state.sessionId = null;
      state.pending = null;
      state.snapshot = null;
      ui.conversation.hidden = true;
      ui.welcome.hidden = false;
      setConnection("Conversation unavailable.");
      showStartError("This conversation is no longer available. Start a new conversation to continue.");
      return;
    }
    setConnection("Reconnecting…", true);
    renderAll();
    connectEvents(state.cursor);
  }
}

function readPending(sessionId) {
  try {
    const value = window.sessionStorage.getItem(storageKey(pendingPrefix, sessionId));
    if (!value) return null;
    const parsed = JSON.parse(value);
    if (!Number.isSafeInteger(parsed.request_id) || typeof parsed.prompt !== "string") return null;
    return parsed;
  } catch {
    return null;
  }
}

async function refreshSnapshot({ eventsArrived = false } = {}) {
  if (!state.sessionId) return;
  const snapshot = await api(`/api/sessions/${encodeURIComponent(state.sessionId)}/snapshot`);
  state.snapshot = snapshot;
  state.active = snapshot.active;
  state.cursor = Number(snapshot.cursor) || 0;
  if (snapshot.session?.id && snapshot.session.id !== state.sessionId && !state.pending) {
    state.sessionId = snapshot.session.id;
    window.sessionStorage.setItem(sessionKey, state.sessionId);
  }
  renderAll({ eventsArrived });
}

function connectEvents(after) {
  if (!state.sessionId || !token) return;
  state.streamAbort?.abort();
  const controller = new AbortController();
  state.streamAbort = controller;
  const generation = ++state.streamGeneration;
  void eventLoop(state.sessionId, Number(after) || 0, controller, generation);
}

async function eventLoop(sessionId, after, controller, generation) {
  let delayMs = 400;
  while (!controller.signal.aborted && generation === state.streamGeneration && state.sessionId === sessionId) {
    try {
      setConnection("Connected to local host.");
      const response = await fetch(
        `/api/sessions/${encodeURIComponent(sessionId)}/events?after=${encodeURIComponent(state.cursor || after)}`,
        {
          headers: { Authorization: `Bearer ${token}`, Accept: "text/event-stream" },
          cache: "no-store",
          credentials: "same-origin",
          signal: controller.signal,
        },
      );
      if (!response.ok || !response.body) throw new HostError("event_stream_unavailable", response.status);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (!controller.signal.aborted) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary;
        while ((boundary = buffer.search(/\r?\n\r?\n/)) !== -1) {
          const frame = buffer.slice(0, boundary);
          const separator = buffer.slice(boundary).match(/^\r?\n\r?\n/)[0];
          buffer = buffer.slice(boundary + separator.length);
          handleSseFrame(frame);
        }
      }
      if (controller.signal.aborted || generation !== state.streamGeneration) return;
      throw new Error("The event stream ended.");
    } catch (error) {
      if (controller.signal.aborted || generation !== state.streamGeneration) return;
      setConnection("Reconnecting…", true);
      try {
        await refreshSnapshot();
      } catch {
        // Keep the active snapshot and draft visible until the host returns.
      }
      await wait(delayMs, controller.signal);
      delayMs = Math.min(delayMs * 2, 5000);
    }
  }
}

function wait(milliseconds, signal) {
  return new Promise((resolve) => {
    const timeout = window.setTimeout(resolve, milliseconds);
    signal.addEventListener("abort", () => {
      window.clearTimeout(timeout);
      resolve();
    }, { once: true });
  });
}

function handleSseFrame(frame) {
  const fields = {};
  const dataLines = [];
  for (const line of frame.split(/\r?\n/)) {
    if (!line || line.startsWith(":")) continue;
    const colon = line.indexOf(":");
    if (colon < 0) continue;
    const key = line.slice(0, colon);
    const value = line.slice(colon + 1).replace(/^ /, "");
    if (key === "data") dataLines.push(value);
    else fields[key] = value;
  }
  if (!dataLines.length) return;
  let data;
  try {
    data = JSON.parse(dataLines.join("\n"));
  } catch {
    return;
  }
  if (fields.event === "reset") {
    state.snapshot = data;
    state.active = data.active;
    state.cursor = Number(fields.id || data.cursor) || 0;
    renderAll({ eventsArrived: true });
    if (state.pending) void reconcilePending();
    return;
  }
  if (fields.event !== "update") return;
  applyHostEvent(data);
}

function applyHostEvent(event) {
  const cursor = Number(event.cursor) || 0;
  if (cursor <= state.cursor) return;
  if (state.cursor && cursor > state.cursor + 1) {
    void refreshSnapshot({ eventsArrived: true }).catch(() => {});
    return;
  }
  state.cursor = cursor;
  if (!state.active && event.kind === "accepted") {
    state.active = {
      turn_id: event.turn_id,
      prompt: ui.prompt.value,
      text: "",
      status: "starting",
      started_at_ms: Date.now(),
      provider: null,
      model: null,
      active_tool: null,
      tools: [],
    };
  }
  if (event.kind === "stream" && event.data) applyStreamEvent(event.data);
  if (event.kind === "outcome") {
    void refreshSnapshot({ eventsArrived: true }).then(() => restoreFailedPrompt()).catch(() => {});
  } else {
    renderAll({ eventsArrived: event.kind === "stream" || event.kind === "accepted" });
  }
}

function applyStreamEvent(event) {
  if (!state.active) return;
  if (state.active.status !== "stopping") state.active.status = "running";
  if (event.event === "turn_start") {
    state.active.provider = event.provider || null;
    state.active.model = event.model || null;
    if (event.turn_index !== undefined && state.snapshot?.session?.display) {
      const metadata = state.snapshot.session.display["web.turn_metadata"] || {};
      metadata[String(event.turn_index)] = {
        provider: event.provider,
        model: event.model,
        started_at_ms: Date.now(),
      };
      state.snapshot.session.display["web.turn_metadata"] = metadata;
    }
  } else if (event.event === "text_delta") {
    state.active.text += event.text || "";
  } else if (event.event === "tool_call") {
    const tool = {
      tool_use_id: event.tool_use_id,
      tool_name: event.tool_name,
      input: event.input,
      status: "running",
    };
    state.active.tools ||= [];
    state.active.tools.push(tool);
    state.active.active_tool = tool;
  } else if (event.event === "tool_result") {
    const tool = [...(state.active.tools || [])].reverse().find((candidate) => candidate.tool_use_id === event.tool_use_id);
    if (tool) {
      tool.status = event.status;
      tool.output = event.output;
    }
    state.active.active_tool = null;
  } else if (event.event === "turn_end") {
    state.active.provider ||= null;
  }
}

function currentStatus() {
  if (state.reconnecting) return { text: "Reconnecting", busy: Boolean(state.active) };
  if (state.submitting) return { text: "Starting", busy: true };
  if (state.active) {
    const activeStatus = state.active.status || "running";
    if (activeStatus === "stopping") return { text: "Stopping", busy: true };
    if (activeStatus === "starting") return { text: "Starting", busy: true };
    return { text: "Running", busy: true };
  }
  const last = state.snapshot?.last_submission;
  if (!last) return { text: "Idle", busy: false };
  if (state.snapshot?.recovery_required || last.status === "incomplete") return { text: "Incomplete", busy: false };
  if (last.status === "running" || last.status === "accepted") return { text: "Incomplete", busy: false };
  if (last.status === "succeeded" && last.outcome?.capped) return { text: "Capped", busy: false };
  if (last.status === "succeeded") return { text: "Succeeded", busy: false };
  if (last.status === "failed") return { text: "Failed", busy: false };
  if (last.status === "stopped") return { text: "Interrupted", busy: false };
  return { text: "Idle", busy: false };
}

function renderStatus() {
  const current = currentStatus();
  ui["turn-status"].textContent = current.text;
  const activeTool = state.active?.active_tool;
  if (activeTool) {
    ui["active-tool"].textContent = `Active tool: ${activeTool.tool_name || "tool"}`;
    ui["active-tool"].hidden = false;
  } else {
    ui["active-tool"].textContent = "";
    ui["active-tool"].hidden = true;
  }
  ui["stop-turn"].hidden = !state.active;
  ui["stop-turn"].disabled = state.active?.status === "stopping" || state.reconnecting;
  ui.send.disabled = current.busy || state.reconnecting || state.submitting || Boolean(state.pending) || !ui.prompt.value.trim();
  ui["new-conversation"].disabled = current.busy || state.reconnecting || state.submitting || Boolean(state.pending);
  const pending = state.pending;
  const canRetry = Boolean(pending && pending.retryAllowed && !current.busy && !state.submitting);
  ui["retry-submission"].hidden = !canRetry;
  ui["retry-submission"].disabled = !canRetry;

  const statusKey = `${current.text}:${activeTool?.tool_name || ""}`;
  if (statusKey !== state.lastAnnouncedStatus) {
    state.lastAnnouncedStatus = statusKey;
    ui["live-status"].textContent = activeTool
      ? `${current.text}. Active tool: ${activeTool.tool_name || "tool"}.`
      : current.text;
  }
}

function renderAll(options = {}) {
  const hasSession = Boolean(state.snapshot && state.sessionId);
  ui.welcome.hidden = hasSession;
  ui.conversation.hidden = !hasSession;
  ui["current-session-card"].hidden = !hasSession;
  if (!hasSession) {
    renderStatus();
    return;
  }
  const session = state.snapshot.session;
  ui["session-title"].textContent = session.name || "Conversation";
  ui["rail-session-name"].textContent = session.name || "Current conversation";
  const cwd = session.cwd || "Workspace not set";
  ui["workspace-label"].textContent = `Workspace: ${cwd}`;
  ui["rail-workspace"].textContent = cwd;
  ui["workspace-warning"].hidden = Boolean((session.turns || []).length || state.snapshot.high_water > 0);
  renderProviderModel();
  renderElapsed();
  renderStatus();
  renderTranscript(options);
}

function renderProviderModel() {
  const active = state.active;
  let provider = active?.provider;
  let model = active?.model;
  if ((!provider || !model) && state.snapshot?.session?.turns?.length) {
    const turns = state.snapshot.session.turns;
    const lastTurn = turns[turns.length - 1];
    const metadata = state.snapshot.session.display?.["web.turn_metadata"]?.[String(lastTurn.turn_index)];
    provider ||= metadata?.provider;
    model ||= metadata?.model;
  }
  ui["provider-model"].textContent = provider && model
    ? `Effective: ${provider} · ${model}`
    : "Effective model and provider appear when leg reports turn metadata.";
}

function renderElapsed() {
  if (!state.active) {
    ui["elapsed-time"].hidden = true;
    ui["elapsed-time"].textContent = "";
    if (elapsedTimer) window.clearInterval(elapsedTimer);
    elapsedTimer = null;
    return;
  }
  ui["elapsed-time"].hidden = false;
  const update = () => {
    const started = Number(state.active?.started_at_ms) || Date.now();
    const seconds = Math.max(0, Math.floor((Date.now() - started) / 1000));
    ui["elapsed-time"].textContent = `Elapsed ${seconds}s`;
  };
  update();
  if (!elapsedTimer) elapsedTimer = window.setInterval(update, 1000);
}

function renderTranscript({ eventsArrived = false } = {}) {
  if (!state.snapshot) return;
  const scroller = ui.transcript;
  const previousTop = scroller.scrollTop;
  const previousBottomGap = scroller.scrollHeight - scroller.clientHeight - previousTop;
  const wasAtBottom = previousBottomGap < 36;
  const anchor = findScrollAnchor(scroller);

  const fragment = document.createDocumentFragment();
  const turns = state.snapshot.session?.turns || [];
  for (const turn of turns) {
    appendTurn(fragment, turn, state.snapshot.session?.display?.["web.turn_metadata"] || {});
  }
  if (state.active) appendActiveTurn(fragment, state.active);
  ui.messages.replaceChildren(fragment);
  ui["empty-transcript"].hidden = ui.messages.childElementCount > 0;

  if (wasAtBottom) {
    scroller.scrollTop = scroller.scrollHeight;
    ui["new-content"].hidden = true;
  } else {
    restoreScrollAnchor(scroller, anchor, previousTop);
    if (eventsArrived) ui["new-content"].hidden = false;
  }
}

function findScrollAnchor(scroller) {
  const point = scroller.getBoundingClientRect().top;
  for (const element of ui.messages.children) {
    const bounds = element.getBoundingClientRect();
    if (bounds.bottom > point) {
      return { key: element.dataset.key, offset: bounds.top - point };
    }
  }
  return null;
}

function restoreScrollAnchor(scroller, anchor, previousTop) {
  if (anchor?.key) {
    const element = [...ui.messages.children].find((node) => node.dataset.key === anchor.key);
    if (element) {
      const point = scroller.getBoundingClientRect().top;
      const offset = element.getBoundingClientRect().top - point;
      scroller.scrollTop += offset - anchor.offset;
      return;
    }
  }
  scroller.scrollTop = previousTop;
}

function appendTurn(parent, turn, metadataByIndex) {
  const metadata = metadataByIndex?.[String(turn.turn_index)] || null;
  appendMessage(parent, "user", turn.prompt || "", `turn-${turn.turn_index}-user`, null);
  const assistant = createMessage("assistant", `turn-${turn.turn_index}-assistant`, metadata);
  const content = assistant.querySelector(".message-content");
  if (turn.reply) appendMarkdown(content, turn.reply);
  else if (turn.failure_message) appendTextParagraph(content, turn.failure_message);
  if (!turn.reply && !turn.failure_message) appendTextParagraph(content, "No final reply was recorded for this turn.");
  for (const tool of turn.tools || []) appendToolSummary(assistant, tool);
  const status = turn.outcome === "succeeded"
    ? "Succeeded"
    : turn.outcome === "failed"
      ? "Failed"
      : turn.outcome === "interrupted"
        ? "Interrupted"
        : "Incomplete";
  appendOutcome(assistant, status);
  parent.append(assistant);
}

function appendActiveTurn(parent, active) {
  const key = active.turn_id || "active";
  appendMessage(parent, "user", active.prompt || "", `active-${key}-user`, null);
  const assistant = createMessage("assistant", `active-${key}-assistant`, {
    provider: active.provider,
    model: active.model,
  });
  const content = assistant.querySelector(".message-content");
  if (active.text) appendMarkdown(content, active.text);
  else appendTextParagraph(content, active.status === "starting" ? "Starting this turn…" : "Waiting for leg's first response…");
  for (const tool of active.tools || []) appendToolSummary(assistant, tool);
  const status = active.status === "stopping" ? "Stopping" : active.status === "starting" ? "Starting" : "Running";
  appendOutcome(assistant, status);
  parent.append(assistant);
}

function appendMessage(parent, role, text, key, metadata) {
  const message = createMessage(role, key, metadata);
  appendMarkdown(message.querySelector(".message-content"), text);
  parent.append(message);
}

function createMessage(role, key, metadata) {
  const article = document.createElement("article");
  article.className = `message message-${role}`;
  article.dataset.key = key;
  const heading = document.createElement("div");
  heading.className = "message-heading";
  const speaker = document.createElement("span");
  speaker.textContent = role === "user" ? "You" : "Leg";
  heading.append(speaker);
  if (metadata?.provider && metadata?.model) {
    const engine = document.createElement("span");
    engine.textContent = `${metadata.provider} · ${metadata.model}`;
    heading.append(engine);
  }
  article.append(heading);
  const content = document.createElement("div");
  content.className = "message-content";
  article.append(content);
  return article;
}

function appendOutcome(article, text) {
  const status = document.createElement("p");
  status.className = "turn-outcome";
  status.textContent = text;
  article.append(status);
}

function appendToolSummary(article, tool) {
  const card = document.createElement("section");
  card.className = "tool-summary";
  const name = document.createElement("strong");
  const stateText = tool.status === "running" ? "running" : tool.status || "finished";
  name.textContent = `Tool activity: ${tool.tool_name || "tool"} · ${stateText}`;
  card.append(name);
  if (tool.output !== undefined || tool.result?.result || tool.result?.error) {
    const raw = tool.output ?? tool.result?.result ?? tool.result?.error ?? "";
    const output = typeof raw === "string" ? raw : JSON.stringify(raw);
    const summary = document.createElement("p");
    if (output.length > 360) {
      summary.dataset.outputChars = String(output.length);
      summary.textContent = `Large tool result summarized (${output.length.toLocaleString()} characters): ${output.slice(0, 220)}…`;
    } else {
      summary.textContent = output;
    }
    card.append(summary);
  }
  article.append(card);
}

function appendTextParagraph(parent, text) {
  const paragraph = document.createElement("p");
  paragraph.textContent = text;
  parent.append(paragraph);
}

function appendMarkdown(parent, source) {
  const lines = String(source).replace(/\r\n?/g, "\n").split("\n");
  let paragraph = [];
  let codeLines = null;
  let list = null;
  const flushParagraph = () => {
    if (!paragraph.length) return;
    const element = document.createElement("p");
    appendInline(element, paragraph.join("\n"));
    parent.append(element);
    paragraph = [];
  };
  const flushList = () => {
    if (!list) return;
    parent.append(list);
    list = null;
  };
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      flushParagraph();
      flushList();
      if (codeLines) {
        const pre = document.createElement("pre");
        const code = document.createElement("code");
        code.textContent = codeLines.join("\n");
        pre.append(code);
        parent.append(pre);
        codeLines = null;
      } else {
        codeLines = [];
      }
      continue;
    }
    if (codeLines) {
      codeLines.push(line);
      continue;
    }
    const heading = line.match(/^(#{1,3})\s+(.+)$/);
    if (heading) {
      flushParagraph();
      flushList();
      const element = document.createElement(heading[1].length === 1 ? "h2" : "h3");
      appendInline(element, heading[2]);
      parent.append(element);
      continue;
    }
    const item = line.match(/^\s*[-*+]\s+(.+)$/);
    if (item) {
      flushParagraph();
      if (!list || list.tagName !== "UL") {
        flushList();
        list = document.createElement("ul");
      }
      const li = document.createElement("li");
      appendInline(li, item[1]);
      list.append(li);
      continue;
    }
    const quote = line.match(/^>\s?(.*)$/);
    if (quote) {
      flushParagraph();
      flushList();
      const blockquote = document.createElement("blockquote");
      appendInline(blockquote, quote[1]);
      parent.append(blockquote);
      continue;
    }
    if (!line.trim()) {
      flushParagraph();
      flushList();
    } else {
      flushList();
      paragraph.push(line);
    }
  }
  if (codeLines) {
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    code.textContent = codeLines.join("\n");
    pre.append(code);
    parent.append(pre);
  }
  flushParagraph();
  flushList();
}

function appendInline(parent, text) {
  const pattern = /(\[[^\]]+\]\([^\s)]+\)|\*\*[^*\n]+\*\*|`[^`\n]+`|\*[^*\n]+\*)/g;
  let offset = 0;
  for (const match of text.matchAll(pattern)) {
    const index = match.index;
    if (index > offset) parent.append(document.createTextNode(text.slice(offset, index)));
    const token = match[0];
    if (token.startsWith("[")) {
      const parsed = token.match(/^\[([^\]]+)\]\(([^\s)]+)\)$/);
      const url = parsed ? safeLink(parsed[2]) : null;
      if (parsed && url) {
        const link = document.createElement("a");
        link.href = url;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.textContent = parsed[1];
        parent.append(link);
      } else {
        parent.append(document.createTextNode(token));
      }
    } else if (token.startsWith("**")) {
      const strong = document.createElement("strong");
      strong.textContent = token.slice(2, -2);
      parent.append(strong);
    } else if (token.startsWith("`")) {
      const code = document.createElement("code");
      code.textContent = token.slice(1, -1);
      parent.append(code);
    } else {
      const emphasis = document.createElement("em");
      emphasis.textContent = token.slice(1, -1);
      parent.append(emphasis);
    }
    offset = index + token.length;
  }
  if (offset < text.length) parent.append(document.createTextNode(text.slice(offset)));
}

function safeLink(raw) {
  if (!/^https?:\/\//i.test(raw)) return null;
  try {
    const url = new URL(raw);
    if (!/^https?:$/.test(url.protocol) || url.username || url.password) return null;
    return url.href;
  } catch {
    return null;
  }
}

function promptHash(prompt) {
  const bytes = new TextEncoder().encode(prompt);
  return crypto.subtle.digest("SHA-256", bytes).then((digest) =>
    [...new Uint8Array(digest)].map((value) => value.toString(16).padStart(2, "0")).join(""),
  );
}

async function submitPrompt({ retry = false } = {}) {
  if (!state.sessionId || state.submitting || state.active || state.reconnecting) return;
  if (retry) {
    if (!state.pending?.retryAllowed) return;
  } else {
    const prompt = ui.prompt.value;
    if (!prompt.trim()) return;
    if (state.pending) return;
    if (!state.snapshot?.session?.cwd) {
      showSendError("Set a workspace before sending. Start a new conversation with an existing absolute folder path.");
      return;
    }
    const requestId = state.snapshot.next_request_id;
    if (!Number.isSafeInteger(requestId)) {
      showSendError("The host cannot provide a new send ID. Reload the conversation before sending.");
      return;
    }
    state.pending = { request_id: requestId, prompt, phase: "sending", retryAllowed: false };
    savePending();
  }

  const pending = state.pending;
  if (!pending) return;
  state.submitting = true;
  showSendError("");
  renderStatus();
  try {
    const receipt = await api(`/api/sessions/${encodeURIComponent(state.sessionId)}/submit`, {
      method: "POST",
      body: { request_id: pending.request_id, prompt: pending.prompt },
    });
    await acceptSubmission(receipt, pending);
  } catch (error) {
    if (error instanceof HostError) {
      if (error.code === "session_busy") {
        state.pending = null;
        savePending();
        showSendError(apiErrorText(error.code));
        await refreshSnapshot().catch(() => {});
      } else if (error.code === "submission_id_conflict" || error.code === "submission_id_stale" || error.code === "submission_id_unexpected") {
        state.pending.phase = "conflict";
        state.pending.retryAllowed = false;
        savePending();
        showSendError(apiErrorText(error.code));
      } else {
        state.pending = null;
        savePending();
        showSendError(apiErrorText(error.code));
      }
    } else {
      state.pending.phase = "unknown";
      state.pending.retryAllowed = false;
      savePending();
      setConnection("Checking send status…", true);
      await reconcilePending();
      if (state.pending) showSendError("The send response was lost. The host status was checked; use Retry same send only if it was not accepted.");
    }
  } finally {
    state.submitting = false;
    renderStatus();
  }
}

async function acceptSubmission(receipt, pending) {
  const oldId = state.sessionId;
  const actualId = receipt.session_id || oldId;
  if (actualId !== oldId) {
    state.streamGeneration += 1;
    state.streamAbort?.abort();
    state.sessionId = actualId;
    window.sessionStorage.setItem(sessionKey, actualId);
    const draft = window.sessionStorage.getItem(storageKey(draftPrefix, oldId));
    if (draft !== null) window.sessionStorage.setItem(storageKey(draftPrefix, actualId), draft);
    window.sessionStorage.removeItem(storageKey(pendingPrefix, oldId));
  }
  const currentPending = state.pending;
  state.pending = null;
  savePending();
  if (ui.prompt.value === pending.prompt) ui.prompt.value = "";
  saveDraft();
  adjustTextarea();
  await api("/api/sessions/select", {
    method: "POST",
    body: { session_id: actualId, tab_id: tabId },
  }).catch(() => {});
  try {
    await refreshSnapshot();
    setConnection("Connected to local host.");
  } catch {
    setConnection("Reconnecting…", true);
  }
  connectEvents(state.cursor);
  if (receipt.status === "failed" || receipt.status === "incomplete" || receipt.status === "stopped") {
    if (!ui.prompt.value.trim()) ui.prompt.value = pending.prompt;
    saveDraft();
    adjustTextarea();
    showSendError("The turn did not complete. Its prompt is kept so you can edit or send it again deliberately.");
  }
  if (currentPending?.request_id !== pending.request_id) return;
}

async function reconcilePending() {
  if (!state.pending || !state.sessionId) return;
  const pending = state.pending;
  try {
    await refreshSnapshot();
  } catch {
    setConnection("Reconnecting…", true);
    return;
  }
  const snapshot = state.snapshot;
  const receipt = snapshot.last_submission;
  if (Number(snapshot.high_water) >= pending.request_id) {
    if (receipt?.request_id === pending.request_id) {
      const expectedHash = await promptHash(pending.prompt);
      if (receipt.prompt_sha256 !== expectedHash) {
        pending.phase = "conflict";
        pending.retryAllowed = false;
        savePending();
        showSendError("This send ID was accepted with different text in another tab. Your draft is preserved; inspect the current conversation before sending again.");
        return;
      }
      const actualId = receipt.session_id || snapshot.session.id;
      if (actualId && actualId !== state.sessionId) {
        state.sessionId = actualId;
        window.sessionStorage.setItem(sessionKey, actualId);
      }
      const completedWithError = ["failed", "incomplete", "stopped"].includes(receipt.status);
      state.pending = null;
      savePending();
      if (!completedWithError && ui.prompt.value === pending.prompt) ui.prompt.value = "";
      if (completedWithError && !ui.prompt.value.trim()) ui.prompt.value = pending.prompt;
      saveDraft();
      adjustTextarea();
      showSendError(completedWithError ? "The turn did not complete. Its prompt is kept so you can edit or send it again deliberately." : "");
      setConnection("Connected to local host.");
      renderAll();
      return;
    }
    pending.phase = "conflict";
    pending.retryAllowed = false;
    savePending();
    showSendError("Another send advanced this conversation before this one could be confirmed. Your draft is preserved; reload before sending again.");
    return;
  }
  if (Number(snapshot.next_request_id) === pending.request_id) {
    pending.phase = "unaccepted";
    pending.retryAllowed = true;
    savePending();
    setConnection("Connected to local host.");
    showSendError("The host has not accepted this send. You can retry it safely with the same ID and text.");
    renderAll();
    return;
  }
  pending.phase = "conflict";
  pending.retryAllowed = false;
  savePending();
  showSendError("The host could not confirm this send. Your draft is preserved; reload the conversation before sending again.");
}

function restoreFailedPrompt() {
  if (!state.snapshot || ui.prompt.value.trim()) return;
  const last = state.snapshot.last_submission;
  if (!last || !["failed", "incomplete", "stopped"].includes(last.status)) return;
  const turns = state.snapshot.session?.turns || [];
  const turn = turns[turns.length - 1];
  if (turn?.prompt) {
    ui.prompt.value = turn.prompt;
    saveDraft();
    adjustTextarea();
    showSendError("The turn did not complete. Its prompt is kept so you can edit or send it again deliberately.");
  }
}

async function stopTurn() {
  if (!state.sessionId || !state.active || state.active.status === "stopping") return;
  state.active.status = "stopping";
  renderAll();
  try {
    await api(`/api/sessions/${encodeURIComponent(state.sessionId)}/stop`, { method: "POST", body: {} });
    await refreshSnapshot();
    setConnection("Connected to local host.");
  } catch (error) {
    try {
      await refreshSnapshot();
      setConnection("Connected to local host.");
    } catch {
      if (state.active) state.active.status = "running";
      renderAll();
      setConnection("Reconnecting…", true);
    }
    showSendError(apiErrorText(error.code || "host_unavailable"));
  }
}

function openNewConversation() {
  if (state.active || state.pending || state.submitting) return;
  state.streamGeneration += 1;
  state.streamAbort?.abort();
  state.sessionId = null;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = null;
  window.sessionStorage.removeItem(sessionKey);
  ui.conversation.hidden = true;
  ui.welcome.hidden = false;
  ui["current-session-card"].hidden = true;
  ui["workspace-input"].value = window.sessionStorage.getItem(workspaceKey) || "";
  clearStartError();
  setConnection("Connected to local host.");
  ui["workspace-input"].focus();
}

function formatPromptChange() {
  adjustTextarea();
  saveDraft();
  renderStatus();
}

ui["start-form"].addEventListener("submit", startConversation);
ui["new-conversation"].addEventListener("click", openNewConversation);
ui.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  void submitPrompt();
});
ui.prompt.addEventListener("input", formatPromptChange);
ui.prompt.addEventListener("compositionstart", () => { state.composition = true; });
ui.prompt.addEventListener("compositionend", () => { state.composition = false; });
ui.prompt.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || (!event.ctrlKey && !event.metaKey)) return;
  if (state.composition || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  void submitPrompt();
});
ui["retry-submission"].addEventListener("click", () => void submitPrompt({ retry: true }));
ui["stop-turn"].addEventListener("click", () => void stopTurn());
ui["new-content"].addEventListener("click", () => {
  ui.transcript.scrollTop = ui.transcript.scrollHeight;
  ui["new-content"].hidden = true;
  ui.transcript.focus({ preventScroll: true });
});
window.addEventListener("online", () => {
  if (state.sessionId) {
    void refreshSnapshot().then(() => connectEvents(state.cursor)).catch(() => {});
  }
});

ui["workspace-input"].value = window.sessionStorage.getItem(workspaceKey) || "";
if (!token) {
  setConnection("Open the launch URL to authorize this tab.");
  showStartError("Open the one-time launch URL printed by leg-web. The browser does not store provider credentials.");
} else if (state.sessionId) {
  setConnection("Connecting to local host…");
  void activateSession(state.sessionId);
} else {
  setConnection("Connected to local host.");
  ui.welcome.hidden = false;
  ui.conversation.hidden = true;
}
