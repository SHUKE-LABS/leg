const tokenKey = "leg-web-launch-token";
const sessionKey = "leg-web-current-session";
const tabKey = "leg-web-tab-id";
const workspaceKey = "leg-web-last-workspace";
const draftPrefix = "leg-web-draft:";
const pendingPrefix = "leg-web-pending:";
const readingPrefix = "leg-web-reading:";
const inspectionPrefix = "leg-web-inspection:";
const INITIAL_TURN_HEIGHT = 280;
const TURN_OVERSCAN = 2;
const TRANSCRIPT_CREDENTIAL_KEY = new RegExp(
  `(?:^|[_-])(?:${[
    "authorization(?:[_-]?(?:header|headers|key|keys|token|tokens))?",
    "auth(?:[_-]?(?:header|headers|key|keys|token|tokens))?",
    "api[_-]?(?:key|secret|token)",
    "access[_-]?(?:key|token)",
    "(?:bearer|id|launch|refresh|session)[_-]?token",
    "client[_-]?secret",
    "secret(?:[_-]?(?:access[_-]?)?key)?",
    "private[_-]?key",
    "signing[_-]?key",
    "credentials?(?:[_-]?key)?",
    "passwords?",
  ].join("|")})$`,
  "i",
);

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
    "session-list",
    "session-list-empty",
    "session-list-no-results",
    "session-list-error",
    "session-filter",
    "rename-form",
    "rename-input",
    "rename-error",
    "cancel-rename",
    "welcome",
    "start-form",
    "workspace-input",
    "setup-error",
    "conversation",
    "session-title",
    "workspace-label",
    "session-guidance",
    "set-workspace-form",
    "recovery-workspace-input",
    "recovery-error",
    "cancel-workspace",
    "provider-model",
    "elapsed-time",
    "turn-status",
    "active-tool",
    "stop-turn",
    "workspace-warning",
    "connection-message",
    "transcript-warnings",
    "transcript",
    "messages",
    "empty-transcript",
    "new-content",
    "transcript-search",
    "transcript-search-prev",
    "transcript-search-next",
    "transcript-search-clear",
    "transcript-search-status",
    "download-transcript",
    "download-status",
    "copy-status",
    "composer",
    "prompt",
    "retry-submission",
    "send",
    "send-error",
    "live-status",
  ].map((id) => [id, document.getElementById(id)]),
);
const platform = navigator.userAgentData?.platform || navigator.platform || "";
ui.prompt.placeholder = /mac/i.test(platform)
  ? "Message — ⌘+Enter to send"
  : "Message — Ctrl+Enter to send";

const state = {
  sessionId: window.sessionStorage.getItem(sessionKey),
  sessions: [],
  listRefreshBusy: false,
  listRefreshAgain: false,
  listRefreshTimer: null,
  snapshot: null,
  active: null,
  cursor: 0,
  streamAbort: null,
  streamGeneration: 0,
  switchGeneration: 0,
  reconnecting: false,
  pending: null,
  composition: false,
  lastAnnouncedStatus: "",
  connectionText: "",
  submitting: false,
  startBusy: false,
  renamingSessionId: null,
  renderedSessionSignature: "",
  transcriptSessionId: null,
  restoreReadingPosition: null,
  expandedTools: new Set(),
  transcriptItems: [],
  transcriptHeights: new Map(),
  transcriptAverageHeight: INITIAL_TURN_HEIGHT,
  transcriptRange: null,
  transcriptRenderQueued: false,
  followTranscriptToBottom: false,
  transcriptSearchMatches: [],
  transcriptSearchIndex: -1,
  preserveDraftRetry: null,
};

let elapsedTimer = null;
const messageRenderState = new WeakMap();

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

function readExpandedTools(sessionId) {
  try {
    const saved = window.sessionStorage.getItem(storageKey(inspectionPrefix, sessionId));
    const keys = JSON.parse(saved || "[]");
    return new Set(Array.isArray(keys) ? keys.filter((key) => typeof key === "string") : []);
  } catch {
    return new Set();
  }
}

function saveExpandedTools() {
  if (!state.sessionId) return;
  window.sessionStorage.setItem(
    storageKey(inspectionPrefix, state.sessionId),
    JSON.stringify([...state.expandedTools]),
  );
}

function sessionName(session) {
  return session.name || `Conversation ${String(session.id).slice(0, 8)}`;
}

function sessionStatus(session) {
  if (session.recovered) return "Recovered · choose a workspace";
  if (session.read_only) return "Read-only · start a new conversation to continue";
  if (sessionWorkspaceMissing(session)) return "Workspace missing · choose a replacement";
  if (!session.cwd) return "Workspace needed · set one to continue";
  if (session.run_state === "active" || session.pending_new_turn) return "Busy · turn in progress";
  if (session.run_state === "unknown") return "Status unavailable · reopen to check";
  const last = session.turns?.at(-1);
  if (last?.outcome === "failed") return "Last turn failed";
  if (last?.outcome === "interrupted") return "Last turn interrupted";
  if (last?.outcome === "incomplete") return "Last turn incomplete";
  return session.ended ? "Ended" : "Ready";
}

function sessionWorkspaceMissing(session) {
  return !session?.cwd || (session.warnings || []).some((warning) => /recorded workspace .* is missing/i.test(warning));
}

function relativeRecency(timestamp) {
  const updated = Number(timestamp);
  if (!Number.isFinite(updated) || updated <= 0) return "Activity time unavailable";
  const seconds = Math.max(0, Math.floor((Date.now() - updated) / 1000));
  if (seconds < 60) return "Updated just now";
  if (seconds < 3600) return `Updated ${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `Updated ${Math.floor(seconds / 3600)}h ago`;
  return `Updated ${Math.floor(seconds / 86400)}d ago`;
}

async function refreshSessionList({ quiet = false } = {}) {
  if (!token) return false;
  if (state.listRefreshBusy) {
    state.listRefreshAgain = true;
    return false;
  }
  state.listRefreshBusy = true;
  try {
    const payload = await api("/api/sessions");
    state.sessions = Array.isArray(payload?.sessions) ? payload.sessions : [];
    ui["session-list-error"].hidden = true;
    ui["session-list-error"].textContent = "";
    renderSessionList();
    return true;
  } catch (error) {
    if (!quiet) {
      ui["session-list-error"].textContent = apiErrorText(error.code || "host_unavailable");
      ui["session-list-error"].hidden = false;
    }
    return false;
  } finally {
    state.listRefreshBusy = false;
    if (state.listRefreshAgain) {
      state.listRefreshAgain = false;
      void refreshSessionList({ quiet: true });
    }
  }
}

function renderSessionList() {
  const filter = ui["session-filter"].value.trim().toLocaleLowerCase();
  const filteredSessions = state.sessions.filter((session) => sessionName(session).toLocaleLowerCase().includes(filter));
  const signature = JSON.stringify({
    selected: state.sessionId,
    filter,
    sessions: state.sessions.map((session) => [
      session.id,
      session.name,
      session.cwd,
      session.updated_at_ms,
      session.recovered,
      session.read_only,
      session.run_state,
      session.pending_new_turn,
      session.ended,
      session.turns?.at(-1)?.outcome,
      sessionStatus(session),
      relativeRecency(session.updated_at_ms),
    ]),
  });
  if (signature === state.renderedSessionSignature) return;
  state.renderedSessionSignature = signature;
  const focused = document.activeElement;
  const focusedId = focused?.dataset?.sessionId;
  const focusedAction = focused?.dataset?.action;
  const fragment = document.createDocumentFragment();
  for (const session of filteredSessions) {
    const item = document.createElement("li");
    item.className = "session-entry";

    const select = document.createElement("button");
    select.type = "button";
    select.className = "session-select";
    select.dataset.sessionId = session.id;
    select.dataset.action = "open";
    select.setAttribute("aria-current", session.id === state.sessionId ? "page" : "false");
    select.setAttribute("aria-label", `Open ${sessionName(session)}. ${sessionStatus(session)}. ${relativeRecency(session.updated_at_ms)}.`);
    select.addEventListener("click", () => void activateSession(session.id));

    const name = document.createElement("strong");
    name.className = "session-entry-name";
    name.textContent = sessionName(session);
    const workspace = document.createElement("span");
    workspace.className = "session-entry-workspace";
    workspace.textContent = session.cwd || "Workspace not set";
    const status = document.createElement("span");
    status.className = "session-entry-status";
    status.textContent = `${sessionStatus(session)} · ${relativeRecency(session.updated_at_ms)}`;
    select.append(name, workspace, status);

    const actions = document.createElement("div");
    actions.className = "session-entry-actions";
    const rename = document.createElement("button");
    rename.type = "button";
    rename.className = "session-rename";
    rename.dataset.sessionId = session.id;
    rename.dataset.action = "rename";
    rename.textContent = "Rename";
    rename.setAttribute("aria-label", `Rename ${sessionName(session)}`);
    rename.addEventListener("click", () => beginRename(session.id));
    actions.append(rename);

    item.append(select, actions);
    fragment.append(item);
  }
  ui["session-list"].replaceChildren(fragment);
  ui["session-list-empty"].hidden = state.sessions.length > 0;
  ui["session-list-no-results"].hidden = !filter || state.sessions.length === 0 || filteredSessions.length > 0;

  if (focusedId && focusedAction) {
    const replacement = [...ui["session-list"].querySelectorAll("[data-session-id]")]
      .find((element) => element.dataset.sessionId === focusedId && element.dataset.action === focusedAction);
    replacement?.focus({ preventScroll: true });
  }
}

function beginRename(sessionId) {
  const session = state.sessions.find((item) => item.id === sessionId);
  if (!session) return;
  state.renamingSessionId = sessionId;
  ui["rename-input"].value = session.name || "";
  ui["rename-error"].hidden = true;
  ui["rename-error"].textContent = "";
  ui["rename-form"].hidden = false;
  ui["rename-input"].focus();
  ui["rename-input"].select();
}

function saveReadingPosition() {
  if (!state.sessionId || state.restoreReadingPosition) return;
  const scroller = ui.transcript;
  const bottomGap = scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop;
  const position = bottomGap < 36
    ? { atBottom: true }
    : { atBottom: false, ...findScrollAnchor(scroller) };
  if (position.atBottom || position.key) {
    window.sessionStorage.setItem(storageKey(readingPrefix, state.sessionId), JSON.stringify(position));
  }
}

function readReadingPosition(sessionId) {
  try {
    const saved = window.sessionStorage.getItem(storageKey(readingPrefix, sessionId));
    if (!saved) return null;
    const position = JSON.parse(saved);
    if (position?.atBottom === true) return { atBottom: true };
    if (typeof position?.key === "string" && Number.isFinite(position.offset)) {
      return { atBottom: false, key: position.key, offset: position.offset };
    }
  } catch {
    // Ignore a damaged local reading-position entry and start at the transcript end.
  }
  return null;
}

function restoreReadingPosition(scroller, position) {
  if (!position || position.atBottom) {
    scroller.scrollTop = scroller.scrollHeight;
    ui["new-content"].hidden = true;
    return;
  }
  const element = [...ui.messages.querySelectorAll(".transcript-turn")]
    .find((node) => node.dataset.key === position.key);
  if (!element) {
    scroller.scrollTop = 0;
    ui["new-content"].hidden = false;
    return;
  }
  const point = scroller.getBoundingClientRect().top;
  const offset = element.getBoundingClientRect().top - point;
  scroller.scrollTop += offset - position.offset;
  ui["new-content"].hidden = false;
}

function saveCurrentSessionView() {
  if (!state.sessionId) return;
  saveDraft();
  savePending();
  saveReadingPosition();
  saveExpandedTools();
}

function renderSessionGuidance() {
  const session = state.snapshot?.session;
  ui["session-guidance"].replaceChildren();
  ui["session-guidance"].hidden = true;
  ui["set-workspace-form"].hidden = true;
  if (!session) return;

  let message = "";
  const needsWorkspace = session.recovered || sessionWorkspaceMissing(session);
  if (session.recovered) {
    message = "This recovered conversation needs its workspace selected before it can continue.";
  } else if (session.run_state === "active" || session.pending_new_turn) {
    message = "This session is busy. Wait for its current turn to finish before sending another message.";
  } else if (session.read_only) {
    message = "This saved transcript is read-only. Start a new conversation to continue working.";
  } else if (sessionWorkspaceMissing(session)) {
    message = "Choose a workspace folder before sending a message in this session.";
  }
  if (!message) return;

  const text = document.createElement("span");
  text.textContent = message;
  ui["session-guidance"].append(text);
  if (needsWorkspace && (!session.read_only || session.recovered)) {
    const choose = document.createElement("button");
    choose.type = "button";
    choose.className = "quiet-button";
    choose.textContent = "Set workspace";
    choose.addEventListener("click", () => {
      ui["set-workspace-form"].hidden = false;
      ui["recovery-workspace-input"].focus();
    });
    ui["session-guidance"].append(choose);
  }
  ui["session-guidance"].hidden = false;
}

function apiErrorText(code) {
  const messages = {
    unauthorized: "This tab is no longer authorized. Open the current launch URL to reconnect.",
    host_shutting_down: "The local host is shutting down. Your draft is kept in this tab.",
    session_busy: "This conversation is already running in another tab. Your message is still here.",
    workspace_required: "Choose an existing workspace folder for this session or start a new conversation.",
    workspace_must_be_absolute: "Enter an absolute path to an existing folder on the host computer.",
    session_read_only: "This conversation cannot be changed because its saved trail could not be read.",
    submission_id_conflict: "This send ID is already associated with different text. Your draft is preserved; reload the conversation before sending again.",
    submission_id_stale: "This send ID has expired. Your draft is preserved; reload the conversation before sending again.",
    submission_id_unexpected: "Another tab advanced this conversation. Your draft is preserved; reload before sending again.",
    catalog_unavailable: "The host could not start a Leg turn. Check that leg and leg-ui-supervisor are installed, then restart leg-web with --leg-bin and --supervisor-bin if needed. Provider settings come from the environment that starts leg-web.",
    invalid_request: "The host rejected this request. Check the entered value and try again.",
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
  const visibleText = text === "Connected to local host." ? "" : text;
  if (state.connectionText !== text) {
    state.connectionText = text;
    ui["connection-state"].textContent = visibleText;
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
  ui.prompt.style.height = "auto";
  const maxHeight = window.innerHeight * 0.4;
  const contentHeight = ui.prompt.scrollHeight;
  ui.prompt.style.height = `${Math.min(contentHeight, maxHeight)}px`;
  ui.prompt.style.overflowY = contentHeight > maxHeight ? "auto" : "hidden";
}

function absoluteWorkspace(value) {
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value);
}

async function startConversation(event) {
  event.preventDefault();
  if (state.startBusy) return;
  const switchGeneration = state.switchGeneration;
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
    await refreshSessionList({ quiet: true });
    if (state.switchGeneration === switchGeneration) await activateSession(session.id);
  } catch (error) {
    showStartError(apiErrorText(error.code || "host_unavailable"));
  } finally {
    state.startBusy = false;
    ui["start-form"].querySelector("button[type=submit]").disabled = false;
  }
}

async function activateSession(sessionId) {
  if (state.sessionId && state.sessionId !== sessionId) saveCurrentSessionView();
  const transcriptChanged = state.transcriptSessionId !== sessionId;
  const switchGeneration = ++state.switchGeneration;
  state.streamGeneration += 1;
  state.streamAbort?.abort();
  state.followTranscriptToBottom = false;
  state.sessionId = sessionId;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = readPending(sessionId);
  state.expandedTools = readExpandedTools(sessionId);
  state.transcriptItems = [];
  state.transcriptHeights = new Map();
  state.transcriptAverageHeight = INITIAL_TURN_HEIGHT;
  state.transcriptRange = null;
  state.restoreReadingPosition = readReadingPosition(sessionId) || { atBottom: true };
  window.sessionStorage.setItem(sessionKey, sessionId);
  if (transcriptChanged) {
    state.transcriptSessionId = sessionId;
    ui.messages.replaceChildren();
    ui["empty-transcript"].hidden = false;
    ui.transcript.scrollTop = 0;
    ui["transcript-search"].value = "";
    state.transcriptSearchMatches = [];
    state.transcriptSearchIndex = -1;
    renderTranscriptSearch();
    ui["copy-status"].hidden = true;
    ui["copy-status"].textContent = "";
    ui["download-status"].textContent = "";
  }
  const savedDraft = window.sessionStorage.getItem(storageKey(draftPrefix, sessionId));
  ui.prompt.value = savedDraft !== null
    ? savedDraft
    : state.pending?.preserveDraft ? "" : state.pending?.prompt || "";
  showSendError("");
  ui["rename-form"].hidden = true;
  ui["set-workspace-form"].hidden = true;
  ui.welcome.hidden = true;
  ui.conversation.hidden = false;
  adjustTextarea();
  const selected = state.sessions.find((session) => session.id === sessionId);
  ui["session-title"].textContent = selected ? sessionName(selected) : "Opening conversation…";
  ui["empty-transcript"].textContent = "Opening conversation…";
  ui["empty-transcript"].hidden = false;
  ui["transcript-warnings"].replaceChildren();
  ui["transcript-warnings"].hidden = true;
  ui.messages.replaceChildren();
  setConnection("Opening conversation…");
  renderSessionList();
  try {
    await api("/api/sessions/select", {
      method: "POST",
      body: { session_id: sessionId, tab_id: tabId },
    });
    const snapshot = await refreshSnapshot({ sessionId, switchGeneration });
    if (!snapshot || state.switchGeneration !== switchGeneration) return;
    setConnection("Connected to local host.");
    connectEvents(state.cursor, state.sessionId, switchGeneration);
    if (state.pending) await reconcilePending({ sessionId: state.sessionId, switchGeneration });
    await refreshSessionList({ quiet: true });
  } catch (error) {
    if (state.switchGeneration !== switchGeneration || state.sessionId !== sessionId) return;
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
      renderSessionList();
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

async function refreshSnapshot({ eventsArrived = false, sessionId = state.sessionId, switchGeneration = state.switchGeneration } = {}) {
  if (!sessionId) return null;
  const snapshot = await api(`/api/sessions/${encodeURIComponent(sessionId)}/snapshot`);
  if (state.sessionId !== sessionId || state.switchGeneration !== switchGeneration) return null;
  const savedId = state.sessionId;
  const boundSessionId = snapshot.session?.id !== savedId && !state.pending
    ? snapshot.session?.id
    : null;
  state.snapshot = snapshot;
  state.active = snapshot.active;
  state.cursor = Number(snapshot.cursor) || 0;
  if (boundSessionId) adoptBoundSession(savedId, boundSessionId, switchGeneration);
  renderAll({ eventsArrived });
  return snapshot;
}

function bindSessionId(oldId, actualId) {
  if (!actualId || oldId === actualId) return;
  for (const prefix of [draftPrefix, pendingPrefix, readingPrefix, inspectionPrefix]) {
    const oldKey = storageKey(prefix, oldId);
    const value = window.sessionStorage.getItem(oldKey);
    if (value !== null && window.sessionStorage.getItem(storageKey(prefix, actualId)) === null) {
      window.sessionStorage.setItem(storageKey(prefix, actualId), value);
    }
    window.sessionStorage.removeItem(oldKey);
  }
  state.expandedTools = new Set([...state.expandedTools].map((key) => {
    try {
      const identity = JSON.parse(key);
      return Array.isArray(identity) && identity[0] === oldId
        ? JSON.stringify([actualId, ...identity.slice(1)])
        : key;
    } catch {
      return key;
    }
  }));
  state.sessionId = actualId;
  if (state.transcriptSessionId === oldId) state.transcriptSessionId = actualId;
  if (state.snapshot?.session?.id === oldId) state.snapshot.session.id = actualId;
  window.sessionStorage.setItem(sessionKey, actualId);
  saveExpandedTools();
}

function adoptBoundSession(oldId, actualId, switchGeneration) {
  if (
    !oldId?.startsWith("draft-") ||
    !actualId ||
    actualId.startsWith("draft-") ||
    !isCurrentSession(oldId, switchGeneration)
  ) return false;
  bindSessionId(oldId, actualId);
  connectEvents(state.cursor, actualId, switchGeneration);
  void api("/api/sessions/select", {
    method: "POST",
    body: { session_id: actualId, tab_id: tabId },
  }).catch(() => {});
  void refreshSessionList({ quiet: true });
  return true;
}

function isCurrentSession(sessionId, switchGeneration) {
  return state.sessionId === sessionId && state.switchGeneration === switchGeneration;
}

function connectEvents(after, sessionId = state.sessionId, switchGeneration = state.switchGeneration) {
  if (!sessionId || !token) return;
  state.streamAbort?.abort();
  const controller = new AbortController();
  state.streamAbort = controller;
  const generation = ++state.streamGeneration;
  void eventLoop(sessionId, Number(after) || 0, controller, generation, switchGeneration);
}

async function eventLoop(sessionId, after, controller, generation, switchGeneration) {
  let delayMs = 400;
  while (!controller.signal.aborted && generation === state.streamGeneration && isCurrentSession(sessionId, switchGeneration)) {
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
          handleSseFrame(frame, sessionId, switchGeneration);
        }
      }
      if (controller.signal.aborted || generation !== state.streamGeneration || !isCurrentSession(sessionId, switchGeneration)) return;
      throw new Error("The event stream ended.");
    } catch (error) {
      if (controller.signal.aborted || generation !== state.streamGeneration || !isCurrentSession(sessionId, switchGeneration)) return;
      setConnection("Reconnecting…", true);
      try {
        await refreshSnapshot({ sessionId, switchGeneration });
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

function handleSseFrame(frame, sessionId, switchGeneration) {
  if (!isCurrentSession(sessionId, switchGeneration)) return;
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
    const boundSessionId = data.session?.id;
    state.snapshot = data;
    state.active = data.active;
    state.cursor = Number(fields.id || data.cursor) || 0;
    if (!state.pending) adoptBoundSession(sessionId, boundSessionId, switchGeneration);
    renderAll({ eventsArrived: true });
    if (state.pending) void reconcilePending();
    return;
  }
  if (fields.event !== "update") return;
  applyHostEvent(data, sessionId, switchGeneration);
}

function applyHostEvent(event, sessionId, switchGeneration) {
  if (!isCurrentSession(sessionId, switchGeneration)) return;
  const cursor = Number(event.cursor) || 0;
  if (cursor <= state.cursor) return;
  if (state.cursor && cursor > state.cursor + 1) {
    void refreshSnapshot({ eventsArrived: true, sessionId, switchGeneration }).catch(() => {});
    return;
  }
  state.cursor = cursor;
  const boundSessionId = event.kind === "stream" && event.data?.event === "turn_start"
    ? event.session_id
    : null;
  if (sessionId.startsWith("draft-") && boundSessionId && !boundSessionId.startsWith("draft-")) {
    adoptBoundSession(sessionId, boundSessionId, switchGeneration);
    void refreshSnapshot({ eventsArrived: true, sessionId: boundSessionId, switchGeneration })
      .catch(() => {});
  }
  if (!state.active && event.kind === "accepted") {
    const requestId = Number(event.data?.request_id);
    const pendingPrompt = Number.isSafeInteger(requestId) && state.pending?.request_id === requestId
      ? state.pending.prompt
      : null;
    const provisional = {
      turn_id: event.turn_id,
      turn_index: null,
      prompt: pendingPrompt,
      text: "",
      status: "starting",
      started_at_ms: Date.now(),
      provider: null,
      model: null,
      active_tool: null,
      tools: [],
    };
    state.active = provisional;
    if (pendingPrompt === null) {
      void refreshSnapshot({ eventsArrived: true, sessionId, switchGeneration }).then((snapshot) => {
        const snapshotSessionId = snapshot?.session?.id || sessionId;
        if (!snapshot || !isCurrentSession(snapshotSessionId, switchGeneration)) return;
        const receipt = snapshot.last_submission;
        const terminal = receipt?.request_id === requestId
          && !["accepted", "running"].includes(receipt.status);
        if (
          !snapshot.active
          && snapshot.turn_id === event.turn_id
          && !terminal
        ) {
          state.active = provisional;
          renderAll({ eventsArrived: true });
        }
      }).catch(() => {});
    }
  }
  if (event.kind === "stream" && event.data) applyStreamEvent(event.data);
  if (event.kind === "outcome") {
    void refreshSnapshot({ eventsArrived: true, sessionId, switchGeneration }).then((snapshot) => {
      if (snapshot && isCurrentSession(sessionId, switchGeneration)) {
        restoreFailedPrompt();
        void refreshSessionList({ quiet: true });
      }
    }).catch(() => {});
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
    state.active.turn_index = Number.isSafeInteger(event.turn_index) ? event.turn_index : null;
    if (event.turn_index !== undefined && state.snapshot?.session?.display) {
      const metadata = state.snapshot.session.display["web.turn_metadata"] || {};
      metadata[String(event.turn_index)] = {
        ...metadata[String(event.turn_index)],
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
      call_observed_at_ms: event.observed_at_ms,
    };
    state.active.tools ||= [];
    state.active.tools.push(tool);
    state.active.active_tool = tool;
  } else if (event.event === "tool_result") {
    const tool = [...(state.active.tools || [])].reverse().find((candidate) => candidate.tool_use_id === event.tool_use_id);
    if (tool) {
      tool.status = event.status;
      tool.output = event.output;
      tool.result_observed_at_ms = event.observed_at_ms;
    }
    state.active.active_tool = null;
  } else if (event.event === "turn_end") {
    state.active.capped = Boolean(event.capped);
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
  const session = state.snapshot?.session;
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
  ui["download-transcript"].disabled = !state.snapshot;
  ui.send.disabled = current.busy || state.reconnecting || state.submitting || Boolean(state.pending) || sessionWorkspaceMissing(session) || Boolean(session?.read_only) || !ui.prompt.value.trim();
  ui["new-conversation"].disabled = current.busy || state.reconnecting || state.submitting || Boolean(state.pending);
  const pending = state.pending;
  const canRetry = Boolean(pending && pending.retryAllowed && !current.busy && !state.submitting);
  ui["retry-submission"].hidden = !canRetry;
  ui["retry-submission"].disabled = !canRetry;
  for (const button of ui.messages.querySelectorAll(".retry-turn-button")) {
    const turnIndex = Number(button.dataset.turnIndex);
    const turn = state.snapshot?.session?.turns?.find((item) => Number(item.turn_index) === turnIndex);
    button.disabled = !canRetryTurn(turn);
  }

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
  renderSessionList();
  if (!hasSession) {
    renderStatus();
    return;
  }
  const session = state.snapshot.session;
  ui["session-title"].dataset.sessionId = session.id;
  ui["session-title"].textContent = sessionName(session);
  const cwd = session.cwd || "Workspace not set";
  ui["workspace-label"].textContent = cwd;
  ui["workspace-label"].title = cwd;
  ui["workspace-warning"].hidden = Boolean((session.turns || []).length || state.snapshot.high_water > 0);
  renderSessionGuidance();
  renderProviderModel();
  renderElapsed();
  renderStatus();
  renderTranscriptWarnings();
  renderTranscript(options);
}

function renderTranscriptWarnings() {
  const warnings = state.snapshot?.session?.warnings || [];
  ui["transcript-warnings"].replaceChildren();
  for (const warning of warnings) {
    const item = document.createElement("p");
    item.textContent = String(warning);
    ui["transcript-warnings"].append(item);
  }
  ui["transcript-warnings"].hidden = warnings.length === 0;
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
  const hasMetadata = Boolean(provider && model);
  ui["provider-model"].textContent = hasMetadata ? `${provider} · ${model}` : "";
  ui["provider-model"].hidden = !hasMetadata;
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

function buildTranscriptItems() {
  const session = state.snapshot?.session;
  if (!session) return [];
  const metadataByIndex = session.display?.["web.turn_metadata"] || {};
  const observationsByIndex = session.display?.["web.tool_observations"] || {};
  const turns = session.turns || [];
  const items = [];
  let activeAttached = false;

  for (const turn of turns) {
    const turnIndex = Number(turn.turn_index);
    const metadata = metadataByIndex[String(turn.turn_index)] || {};
    let live = null;
    if (state.active && Number.isSafeInteger(state.active.turn_index)) {
      live = state.active.turn_index === turnIndex ? state.active : null;
    } else if (
      state.active &&
      !activeAttached &&
      turn === turns.at(-1) &&
      turn.outcome === "incomplete" &&
      turn.prompt === state.active.prompt
    ) {
      live = state.active;
    }
    if (live) activeAttached = true;
    const key = `turn-${turn.turn_index}-user`;
    items.push({
      key,
      assistantKey: `turn-${turn.turn_index}-assistant`,
      turnIndex,
      prompt: turn.prompt || "",
      reply: live?.text || turn.reply || "",
      failureMessage: turn.failure_message || "",
      outcome: turn.outcome || "incomplete",
      active: live,
      provider: live?.provider || metadata.provider,
      model: live?.model || metadata.model,
      capped: Boolean(live?.capped || metadata.capped),
      tools: mergeTurnTools({
        sessionId: session.id,
        turnIndex,
        turn,
        live,
        observations: observationsByIndex[String(turn.turn_index)] || {},
      }),
    });
  }

  if (state.active && !activeAttached) {
    const turnIndex = Number.isSafeInteger(state.active.turn_index) ? state.active.turn_index : null;
    const metadata = turnIndex === null ? null : metadataByIndex[String(turnIndex)] || null;
    const observation = turnIndex === null ? {} : observationsByIndex[String(turnIndex)] || {};
    items.push({
      key: turnIndex === null
        ? `active-${state.active.turn_id || "current"}-user`
        : `turn-${turnIndex}-user`,
      assistantKey: turnIndex === null
        ? `active-${state.active.turn_id || "current"}-assistant`
        : `turn-${turnIndex}-assistant`,
      turnIndex,
      prompt: state.active.prompt || "",
      reply: state.active.text || "",
      failureMessage: "",
      outcome: "incomplete",
      active: state.active,
      provider: state.active.provider || metadata?.provider,
      model: state.active.model || metadata?.model,
      capped: Boolean(state.active.capped || metadata?.capped),
      tools: mergeTurnTools({
        sessionId: session.id,
        turnIndex: turnIndex ?? `active:${state.active.turn_id || "current"}`,
        turn: { tools: [] },
        live: state.active,
        observations: observation,
      }),
    });
  }
  return items;
}

function mergeTurnTools({ sessionId, turnIndex, turn, live, observations }) {
  const toolsById = new Map();
  const outcome = turn.outcome || "incomplete";
  const currentlyRunning = Boolean(live && ["starting", "running", "stopping"].includes(live.status));
  for (const saved of turn.tools || []) {
    const id = String(saved.tool_use_id ?? "");
    const result = saved.result || null;
    const status = result?.status || (currentlyRunning
      ? "pending"
      : outcome === "interrupted"
        ? "interrupted"
        : ["incomplete", "failed"].includes(outcome)
          ? "missing"
          : "unavailable");
    const observation = observations && Object.hasOwn(observations, id) ? observations[id] : {};
    const failedResult = result && ["failed", "denied"].includes(result.status);
    toolsById.set(id, {
      id,
      name: saved.tool_name || "tool",
      input: saved.input,
      status,
      output: failedResult ? undefined : result?.result,
      error: result?.error ?? (failedResult ? result?.result : undefined),
      callObservedAt: observation.call_observed_at_ms,
      resultObservedAt: observation.result_observed_at_ms,
      identity: disclosureKey(sessionId, turnIndex, id),
    });
  }
  for (const streamed of live?.tools || []) {
    const id = String(streamed.tool_use_id ?? "");
    const previous = toolsById.get(id) || {
      id,
      name: streamed.tool_name || "tool",
      input: streamed.input,
      status: "pending",
      output: undefined,
      error: undefined,
      callObservedAt: undefined,
      resultObservedAt: undefined,
      identity: disclosureKey(sessionId, turnIndex, id),
    };
    const hasSavedResult = previous.status === "completed" || previous.status === "failed" || previous.status === "denied";
    const streamedStatus = streamed.status === "running" ? "pending" : streamed.status || previous.status;
    const streamedFailure = ["failed", "denied"].includes(streamedStatus);
    toolsById.set(id, {
      ...previous,
      name: streamed.tool_name || previous.name,
      input: streamed.input === undefined ? previous.input : streamed.input,
      status: hasSavedResult ? previous.status : streamedStatus,
      output: streamed.output === undefined || streamedFailure ? previous.output : streamed.output,
      error: streamed.error === undefined
        ? streamedFailure && streamed.output !== undefined ? streamed.output : previous.error
        : streamed.error,
      callObservedAt: streamed.call_observed_at_ms ?? previous.callObservedAt,
      resultObservedAt: streamed.result_observed_at_ms ?? previous.resultObservedAt,
    });
  }
  return [...toolsById.values()];
}

function disclosureKey(sessionId, turnIndex, toolUseId) {
  return JSON.stringify([sessionId, turnIndex, toolUseId]);
}

function renderTranscript({ eventsArrived = false, force = true } = {}) {
  if (!state.snapshot) return;
  const scroller = ui.transcript;
  const items = buildTranscriptItems();
  const previousTop = scroller.scrollTop;
  const previousBottomGap = scroller.scrollHeight - scroller.clientHeight - previousTop;
  const wasAtBottom = state.followTranscriptToBottom || previousBottomGap < 36;
  const anchor = findScrollAnchor(scroller);
  const restore = state.restoreReadingPosition;
  state.transcriptItems = items;

  if (state.transcriptSessionId !== state.sessionId) {
    ui.messages.replaceChildren();
    state.transcriptHeights.clear();
    state.transcriptAverageHeight = INITIAL_TURN_HEIGHT;
    state.transcriptRange = null;
    state.transcriptSessionId = state.sessionId;
  }

  let windowScrollTop = previousTop;
  if (restore?.atBottom) {
    windowScrollTop = Math.max(0, estimatedTranscriptHeight(items) - scroller.clientHeight);
  } else if (restore?.key) {
    const restoreIndex = items.findIndex((item) => item.key === restore.key);
    if (restoreIndex >= 0) {
      windowScrollTop = Math.max(0, estimatedHeightBefore(items, restoreIndex) - restore.offset);
      scroller.scrollTop = windowScrollTop;
      windowScrollTop = scroller.scrollTop;
    }
  } else if (wasAtBottom) {
    windowScrollTop = Math.max(0, estimatedTranscriptHeight(items) - scroller.clientHeight);
  }

  let { start, end } = transcriptWindow(items, windowScrollTop, scroller.clientHeight);
  const focusedTurn = document.activeElement?.closest?.(".transcript-turn");
  if (focusedTurn) {
    const focusedIndex = items.findIndex((item) => item.key === focusedTurn.dataset.key);
    if (focusedIndex >= 0) {
      if (focusedIndex < start || focusedIndex >= end) {
        scroller.focus({ preventScroll: true });
      } else {
        start = Math.min(start, focusedIndex);
        end = Math.max(end, focusedIndex + 1);
      }
    }
  }
  const sameRange = state.transcriptRange?.start === start && state.transcriptRange?.end === end;
  if (!force && sameRange) {
    if (restore) {
      restoreReadingPosition(scroller, restore);
      state.restoreReadingPosition = null;
    } else if (wasAtBottom) {
      scroller.scrollTop = scroller.scrollHeight;
      ui["new-content"].hidden = true;
    }
    renderTranscriptSearch();
    if (!state.active) state.followTranscriptToBottom = false;
    return;
  }

  const focusedKey = ui.messages.contains(document.activeElement)
    ? document.activeElement.dataset?.focusKey || null
    : null;
  const existing = new Map(
    [...ui.messages.querySelectorAll(":scope > .transcript-turn")]
      .map((turn) => [turn.dataset.key, turn]),
  );

  if (sameRange) {
    const visibleKeys = new Set(items.slice(start, end).map((item) => item.key));
    for (const [key, turn] of existing) {
      if (!visibleKeys.has(key)) turn.remove();
    }
    for (let index = start; index < end; index += 1) {
      appendTurn(ui.messages, items[index], index < items.length - 1, existing.get(items[index].key));
    }
    measureTranscriptTurns();
    refreshTranscriptSpacers();
    if (restore) {
      restoreReadingPosition(scroller, restore);
      state.restoreReadingPosition = null;
    } else if (wasAtBottom) {
      scroller.scrollTop = scroller.scrollHeight;
      ui["new-content"].hidden = true;
    } else {
      restoreScrollAnchor(scroller, anchor, previousTop);
      if (eventsArrived) ui["new-content"].hidden = false;
    }
    if (focusedKey) {
      [...ui.messages.querySelectorAll("[data-focus-key]")]
        .find((element) => element.dataset.focusKey === focusedKey)
        ?.focus({ preventScroll: true });
    }
    renderTranscriptSearch();
    if (!state.active) state.followTranscriptToBottom = false;
    return;
  }

  const fragment = document.createDocumentFragment();
  const topSpacer = createTranscriptSpacer(estimatedHeightBefore(items, start), "top");
  fragment.append(topSpacer);
  for (let index = start; index < end; index += 1) {
    appendTurn(fragment, items[index], index < items.length - 1, existing.get(items[index].key));
  }
  const bottomSpacer = createTranscriptSpacer(
    Math.max(0, estimatedTranscriptHeight(items) - estimatedHeightBefore(items, end)),
    "bottom",
  );
  fragment.append(bottomSpacer);
  ui.messages.replaceChildren(fragment);
  ui["empty-transcript"].hidden = items.length > 0;
  if (!items.length) ui["empty-transcript"].textContent = "Your conversation will appear here.";
  state.transcriptRange = { start, end };
  measureTranscriptTurns();
  refreshTranscriptSpacers();

  if (restore) {
    restoreReadingPosition(scroller, restore);
    state.restoreReadingPosition = null;
  } else if (wasAtBottom) {
    scroller.scrollTop = scroller.scrollHeight;
    ui["new-content"].hidden = true;
  } else {
    restoreScrollAnchor(scroller, anchor, previousTop);
    if (eventsArrived) ui["new-content"].hidden = false;
  }
  if (focusedKey) {
    const replacement = [...ui.messages.querySelectorAll("[data-focus-key]")]
      .find((element) => element.dataset.focusKey === focusedKey);
    replacement?.focus({ preventScroll: true });
  }
  renderTranscriptSearch();
  if (!state.active) state.followTranscriptToBottom = false;
}

function estimatedHeightBefore(items, endIndex) {
  let height = 0;
  for (let index = 0; index < endIndex; index += 1) {
    height += state.transcriptHeights.get(items[index].key) || state.transcriptAverageHeight;
  }
  return height;
}

function estimatedTranscriptHeight(items) {
  return estimatedHeightBefore(items, items.length);
}

function transcriptIndexAtOffset(items, offset) {
  let height = 0;
  for (let index = 0; index < items.length; index += 1) {
    height += state.transcriptHeights.get(items[index].key) || state.transcriptAverageHeight;
    if (height >= offset) return index;
  }
  return Math.max(0, items.length - 1);
}

function transcriptWindow(items, scrollTop, viewportHeight) {
  if (items.length <= 1) return { start: 0, end: items.length };
  const viewport = Math.max(320, viewportHeight || 600);
  const startAt = transcriptIndexAtOffset(items, Math.max(0, scrollTop - viewport));
  const endAt = transcriptIndexAtOffset(items, scrollTop + viewport * 2);
  return {
    start: Math.max(0, startAt - TURN_OVERSCAN),
    end: Math.min(items.length, Math.max(startAt + 1, endAt + TURN_OVERSCAN + 1)),
  };
}

function createTranscriptSpacer(height, position) {
  const spacer = document.createElement("div");
  spacer.className = `transcript-spacer transcript-spacer-${position}`;
  spacer.setAttribute("aria-hidden", "true");
  spacer.style.height = `${Math.max(0, height)}px`;
  return spacer;
}

function measureTranscriptTurns() {
  let total = 0;
  let count = 0;
  for (const turn of ui.messages.querySelectorAll(".transcript-turn")) {
    const margin = Number.parseFloat(window.getComputedStyle(turn).marginBottom) || 0;
    const height = turn.getBoundingClientRect().height + margin;
    if (height > 0) {
      state.transcriptHeights.set(turn.dataset.key, height);
      total += height;
      count += 1;
    }
  }
  if (count) state.transcriptAverageHeight = Math.max(120, total / count);
}

function refreshTranscriptSpacers() {
  const range = state.transcriptRange;
  if (!range) return;
  const top = ui.messages.querySelector(".transcript-spacer-top");
  const bottom = ui.messages.querySelector(".transcript-spacer-bottom");
  if (top) top.style.height = `${estimatedHeightBefore(state.transcriptItems, range.start)}px`;
  if (bottom) {
    bottom.style.height = `${Math.max(
      0,
      estimatedTranscriptHeight(state.transcriptItems) - estimatedHeightBefore(state.transcriptItems, range.end),
    )}px`;
  }
}

function scheduleTranscriptRender() {
  if (state.transcriptRenderQueued) return;
  state.transcriptRenderQueued = true;
  window.requestAnimationFrame(() => {
    state.transcriptRenderQueued = false;
    renderTranscript({ force: false });
  });
}

function searchableTranscriptFields() {
  const fields = [];
  const add = (key, messageKey, label, text, toolIndex = null) => {
    if (typeof text === "string" && text.length) fields.push({ key, messageKey, label, text, toolIndex });
  };
  for (const item of state.transcriptItems) {
    add(item.key, `${item.key}-message`, "Prompt", item.prompt);
    add(item.key, item.assistantKey, "Reply", item.reply);
    add(item.key, item.assistantKey, "Failure details", item.failureMessage);
    add(item.key, item.assistantKey, "Outcome", item.outcome);
    for (const [toolIndex, tool] of (item.tools || []).entries()) {
      add(item.key, item.assistantKey, "Tool name", tool.name, toolIndex);
      if (tool.input !== undefined) add(item.key, item.assistantKey, "Tool input", toolLiteralText(tool.input), toolIndex);
      add(item.key, item.assistantKey, "Tool status", tool.status, toolIndex);
      if (tool.output !== undefined) add(item.key, item.assistantKey, "Tool result", toolLiteralText(tool.output), toolIndex);
      if (tool.error !== undefined) add(item.key, item.assistantKey, "Tool error", toolLiteralText(tool.error), toolIndex);
    }
  }
  return fields;
}

function collectTranscriptMatches(query) {
  const needle = query.toLocaleLowerCase();
  if (!needle) return [];
  const matches = [];
  for (const field of searchableTranscriptFields()) {
    const haystack = field.text.toLocaleLowerCase();
    let from = 0;
    while (from <= haystack.length - needle.length) {
      const start = haystack.indexOf(needle, from);
      if (start < 0) break;
      matches.push({ ...field, start, end: start + needle.length });
      from = start + Math.max(needle.length, 1);
    }
  }
  return matches;
}

function renderTranscriptSearch({ reset = false, navigate = false } = {}) {
  const query = ui["transcript-search"].value.trim();
  state.transcriptSearchMatches = collectTranscriptMatches(query);
  const matches = state.transcriptSearchMatches;
  const count = matches.length;
  for (const turn of ui.messages.querySelectorAll(".transcript-turn")) turn.classList.remove("search-match-current");
  ui["transcript-search-clear"].disabled = !ui["transcript-search"].value;
  ui["transcript-search-prev"].disabled = !count;
  ui["transcript-search-next"].disabled = !count;

  if (!query) {
    state.transcriptSearchIndex = -1;
    ui["transcript-search-status"].textContent = "Enter text to search this transcript.";
    return;
  }
  if (!count) {
    state.transcriptSearchIndex = -1;
    ui["transcript-search-status"].textContent = "No matches found in this transcript.";
    return;
  }
  if (reset || state.transcriptSearchIndex < 0) state.transcriptSearchIndex = 0;
  state.transcriptSearchIndex %= count;
  const match = matches[state.transcriptSearchIndex];
  const excerptStart = Math.max(0, match.start - 36);
  const excerptEnd = Math.min(match.text.length, match.end + 52);
  const excerpt = `${excerptStart ? "…" : ""}${match.text.slice(excerptStart, excerptEnd).replace(/\s+/g, " ")}${excerptEnd < match.text.length ? "…" : ""}`;
  ui["transcript-search-status"].textContent = `Match ${state.transcriptSearchIndex + 1} of ${count} · ${match.label}: ${excerpt}`;

  const turn = [...ui.messages.querySelectorAll(".transcript-turn")].find((node) => node.dataset.key === match.key);
  if (turn) {
    turn.classList.add("search-match-current");
    [...turn.querySelectorAll("article.message")]
      .find((node) => node.dataset.key === match.messageKey)
      ?.classList.add("search-match-current");
    if (navigate) scrollToSearchMatch(match);
  } else if (navigate) {
    scrollToSearchMatch(match);
  }
}

function scrollToSearchMatch(match) {
  state.followTranscriptToBottom = false;
  const index = state.transcriptItems.findIndex((item) => item.key === match.key);
  if (index < 0) return;
  const query = ui["transcript-search"].value.trim();
  const searchIndex = state.transcriptSearchIndex;
  ui.transcript.scrollTop = Math.max(0, estimatedHeightBefore(state.transcriptItems, index));
  renderTranscript({ force: true });
  window.requestAnimationFrame(() => {
    if (ui["transcript-search"].value.trim() !== query || state.transcriptSearchIndex !== searchIndex) return;
    const turn = [...ui.messages.querySelectorAll(".transcript-turn")].find((node) => node.dataset.key === match.key);
    if (!turn) return;
    const target = match.toolIndex === null
      ? turn
      : turn.querySelector(`.tool-inspector[data-tool-index="${match.toolIndex}"]`) || turn;
    const scrollerBounds = ui.transcript.getBoundingClientRect();
    const targetBounds = target.getBoundingClientRect();
    ui.transcript.scrollTop += targetBounds.top - scrollerBounds.top - (ui.transcript.clientHeight - targetBounds.height) / 2;
    scheduleTranscriptRender();
  });
}

function moveTranscriptSearch(direction) {
  const count = state.transcriptSearchMatches.length;
  if (!count) return;
  state.transcriptSearchIndex = (state.transcriptSearchIndex + direction + count) % count;
  renderTranscriptSearch({ navigate: true });
}

function transcriptExportValue(value) {
  if (Array.isArray(value)) return value.map(transcriptExportValue);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key]) => !isTranscriptCredentialKey(key))
      .map(([key, nested]) => [key, transcriptExportValue(nested)]),
  );
}

function isTranscriptCredentialKey(key) {
  const normalizedKey = key.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
  const hasTokenField = /(?:^|[^a-z0-9])tokens?(?:$|[^a-z0-9])/.test(normalizedKey);
  return hasTokenField || TRANSCRIPT_CREDENTIAL_KEY.test(normalizedKey);
}

function allowedTranscriptExport(snapshot) {
  const turns = snapshot?.session?.turns || [];
  return {
    schema: "leg-web.transcript/v1",
    turns: turns.map((turn) => ({
      turn_index: turn.turn_index,
      prompt: turn.prompt,
      ...(typeof turn.reply === "string" ? { reply: turn.reply } : {}),
      ...(typeof turn.failure_message === "string" ? { failure_message: turn.failure_message } : {}),
      outcome: turn.outcome,
      tools: (turn.tools || []).map((tool) => ({
        tool_name: tool.tool_name,
        input: transcriptExportValue(tool.input),
        ...(tool.result ? {
          result: {
            status: tool.result.status,
            ...(typeof tool.result.result === "string" ? { result: transcriptExportValue(tool.result.result) } : {}),
            ...(typeof tool.result.error === "string" ? { error: transcriptExportValue(tool.result.error) } : {}),
          },
        } : {}),
      })),
    })),
  };
}

function downloadTranscript() {
  if (!state.snapshot) return;
  try {
    const content = `${JSON.stringify(allowedTranscriptExport(state.snapshot), null, 2)}\n`;
    const url = URL.createObjectURL(new Blob([content], { type: "application/json" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "leg-web-transcript.json";
    link.hidden = true;
    document.body.append(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
    ui["download-status"].textContent = "Transcript download started.";
  } catch {
    ui["download-status"].textContent = "Transcript download could not be created.";
  }
}

function findScrollAnchor(scroller) {
  const point = scroller.getBoundingClientRect().top;
  for (const element of ui.messages.querySelectorAll(".transcript-turn")) {
    const bounds = element.getBoundingClientRect();
    if (bounds.bottom > point) {
      return { key: element.dataset.key, offset: bounds.top - point };
    }
  }
  return null;
}

function restoreScrollAnchor(scroller, anchor, previousTop) {
  if (anchor?.key) {
    const element = [...ui.messages.querySelectorAll(".transcript-turn")]
      .find((node) => node.dataset.key === anchor.key);
    if (element) {
      const point = scroller.getBoundingClientRect().top;
      const offset = element.getBoundingClientRect().top - point;
      scroller.scrollTop += offset - anchor.offset;
      return;
    }
  }
  scroller.scrollTop = previousTop;
}

function appendTurn(parent, item, hasLaterTurn, existingWrapper = null) {
  const wrapper = existingWrapper || document.createElement("section");
  wrapper.className = hasLaterTurn ? "transcript-turn transcript-turn-spaced" : "transcript-turn";
  wrapper.dataset.key = item.key;
  if (item.turnIndex !== null) wrapper.dataset.turnIndex = String(item.turnIndex);
  const existingMessages = new Map([...wrapper.children].map((node) => [node.dataset.key, node]));
  const user = getMessage(existingMessages, "user", `${item.key}-message`);
  if (item.active && typeof item.active.prompt !== "string") {
    updateMessageBody(user, "Submitted prompt is loading…", "placeholder");
  } else {
    updateMessageBody(user, item.prompt || "");
  }

  const assistant = getMessage(existingMessages, "assistant", item.assistantKey);
  updateMessageMetadata(assistant, { provider: item.provider, model: item.model });
  if (item.reply) updateMessageBody(assistant, item.reply);
  else if (item.failureMessage) updateMessageBody(assistant, item.failureMessage, "text");
  else if (item.active?.status === "starting") updateMessageBody(assistant, "Starting this turn…", "text");
  else if (item.active) updateMessageBody(assistant, "Waiting for Leg's first response…", "text");
  else updateMessageBody(assistant, "No final reply was recorded for this turn.", "text");

  const status = item.active?.status === "stopping"
    ? "Stopping"
    : item.active?.status === "starting"
      ? "Starting"
      : item.active
        ? "Running"
        : item.outcome === "succeeded"
          ? "Succeeded"
          : item.outcome === "failed"
            ? "Failed"
            : item.outcome === "interrupted"
              ? "Interrupted"
              : "Incomplete";
  updateMessageDetails(assistant, item, status);

  const desired = [user, assistant];
  const desiredSet = new Set(desired);
  for (const node of [...wrapper.children]) {
    if (!desiredSet.has(node)) node.remove();
  }
  let current = wrapper.firstElementChild;
  for (const node of desired) {
    if (node === current) current = current.nextElementSibling;
    else wrapper.insertBefore(node, current);
  }
  if (parent !== ui.messages) {
    parent.append(wrapper);
  } else if (wrapper.parentElement !== parent) {
    const bottomSpacer = parent.querySelector(":scope > .transcript-spacer-bottom");
    parent.insertBefore(wrapper, bottomSpacer);
  }
}

function getMessage(existing, role, key) {
  return existing.get(key) || createMessage(role, key);
}

function createMessage(role, key) {
  const article = document.createElement("article");
  article.className = `message message-${role}`;
  article.dataset.key = key;
  const heading = document.createElement("div");
  heading.className = "message-heading";
  const speaker = document.createElement("span");
  speaker.textContent = role === "user" ? "You" : "Leg";
  heading.append(speaker);
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "message-copy-button";
  copy.textContent = role === "user" ? "Copy prompt" : "Copy reply";
  copy.addEventListener("click", (event) => {
    event.stopPropagation();
    const text = messageRenderState.get(article)?.bodyText || "";
    void copyTranscriptText(text, article);
  });
  heading.append(copy);
  article.append(heading);
  const content = document.createElement("div");
  content.className = "message-content";
  article.append(content);
  messageRenderState.set(article, { bodyMode: null, bodyText: null, metadata: null, details: null });
  return article;
}

function updateMessageMetadata(article, metadata) {
  const signature = metadata?.provider && metadata?.model
    ? `${metadata.provider}\u0000${metadata.model}`
    : "";
  const rendered = messageRenderState.get(article);
  if (rendered.metadata === signature) return;
  rendered.metadata = signature;
  const heading = article.querySelector(".message-heading");
  let engine = heading.querySelector(".message-engine");
  if (!signature) {
    engine?.remove();
    return;
  }
  if (!engine) {
    engine = document.createElement("span");
    engine.className = "message-engine";
    heading.insertBefore(engine, heading.querySelector(".message-copy-button"));
  }
  engine.textContent = `${metadata.provider} · ${metadata.model}`;
}

function updateMessageBody(article, text, mode = "markdown") {
  const rendered = messageRenderState.get(article);
  const value = String(text);
  if (rendered.bodyMode === mode && rendered.bodyText === value) return;
  rendered.bodyMode = mode;
  rendered.bodyText = value;
  const content = article.querySelector(".message-content");
  content.replaceChildren();
  if (mode === "markdown") appendMarkdown(content, value);
  else if (value) appendTextParagraph(content, value);
}

function updateMessageDetails(article, item, status) {
  const sourceTurn = Number.isSafeInteger(item.turnIndex)
    ? state.snapshot?.session?.turns?.find((turn) => Number(turn.turn_index) === item.turnIndex)
    : null;
  const signature = JSON.stringify([item.tools, status, item.capped, item.outcome, Boolean(item.active), canRetryTurn(sourceTurn)]);
  const rendered = messageRenderState.get(article);
  if (rendered.details === signature) return;
  rendered.details = signature;
  for (const detail of article.querySelectorAll(".tool-summary, .tool-inspector, .turn-outcome, .turn-warning, .retry-turn-control")) {
    detail.remove();
  }
  for (const [toolIndex, tool] of item.tools.entries()) appendToolInspector(article, tool, toolIndex);
  appendOutcome(article, status);
  if (item.capped) {
    const warning = document.createElement("p");
    warning.className = "turn-warning";
    warning.textContent = "Output capped by Leg. The available reply and tool results are shown above.";
    article.append(warning);
  }
  if (!item.active && ["incomplete", "interrupted", "failed"].includes(item.outcome)) {
    const warning = document.createElement("p");
    warning.className = "turn-warning";
    warning.textContent = item.outcome === "interrupted"
      ? "Turn interrupted. Tool calls without results are marked Interrupted."
      : `Turn ${item.outcome}. Tool calls without results are marked Missing outcome.`;
    article.append(warning);
  }
  if (sourceTurn && ["failed", "interrupted"].includes(sourceTurn.outcome)) {
    appendRetryTurnControl(article, sourceTurn);
  }
}

function appendOutcome(article, text) {
  const status = document.createElement("p");
  status.className = "turn-outcome";
  status.textContent = text;
  article.append(status);
}

function canRetryTurn(turn) {
  const session = state.snapshot?.session;
  const lastStatus = state.snapshot?.last_submission?.status;
  return Boolean(
    turn && ["failed", "interrupted"].includes(turn.outcome) &&
    state.snapshot && !state.snapshot.recovery_required && Number.isSafeInteger(state.snapshot.next_request_id) &&
    !["running", "accepted"].includes(lastStatus) &&
    !currentStatus().busy && !state.active && !state.reconnecting && !state.submitting && !state.pending &&
    !session?.read_only && !sessionWorkspaceMissing(session) &&
    session?.run_state !== "active" && !session?.pending_new_turn
  );
}

function appendRetryTurnControl(article, turn) {
  const control = document.createElement("div");
  control.className = "retry-turn-control";
  const warning = document.createElement("p");
  warning.className = "retry-turn-warning";
  warning.textContent = "Retry sends this prompt again and may repeat tool side effects.";
  const retry = document.createElement("button");
  retry.type = "button";
  retry.className = "retry-turn-button";
  retry.dataset.turnIndex = String(turn.turn_index);
  retry.textContent = "Retry turn";
  retry.setAttribute("aria-label", `Retry turn ${turn.turn_index}`);
  retry.disabled = !canRetryTurn(turn);
  retry.addEventListener("click", () => submitRecordedTurn(turn));
  control.append(warning, retry);
  article.append(control);
}

function submitRecordedTurn(turn) {
  const currentTurn = state.snapshot?.session?.turns?.find((item) => Number(item.turn_index) === Number(turn.turn_index));
  if (!currentTurn || currentTurn.prompt !== turn.prompt || !canRetryTurn(currentTurn)) return;
  void submitPrompt({ recordedPrompt: currentTurn.prompt, preserveDraft: true });
}

function toolStatusLabel(status) {
  if (status === "completed") return "Completed";
  if (status === "failed") return "Failed";
  if (status === "denied") return "Denied";
  if (status === "pending") return "Pending";
  if (status === "interrupted") return "Interrupted";
  if (status === "missing") return "Missing outcome";
  return "Outcome unavailable";
}

function appendToolInspector(article, tool, toolIndex) {
  const card = document.createElement("section");
  card.className = `tool-inspector tool-inspector-${tool.status}`;
  card.dataset.toolIndex = String(toolIndex);
  const disclosureId = `tool-details-${Math.random().toString(36).slice(2)}`;
  const expanded = state.expandedTools.has(tool.identity);
  const button = document.createElement("button");
  button.type = "button";
  button.className = "tool-disclosure";
  button.dataset.focusKey = tool.identity;
  button.setAttribute("aria-expanded", String(expanded));
  button.setAttribute("aria-controls", disclosureId);
  button.textContent = `Tool ${tool.name} · ${tool.id || "ID unavailable"} · ${toolStatusLabel(tool.status)} · ${expanded ? "Hide details" : "Show details"}`;

  const preview = document.createElement("p");
  preview.className = "tool-preview";
  if (tool.output !== undefined || tool.error !== undefined) {
    const raw = tool.error ?? tool.output;
    const output = toolLiteralText(raw);
    const prefix = tool.error !== undefined || ["failed", "denied"].includes(tool.status)
      ? "Error preview"
      : "Output preview";
    if (output.length > 180) preview.dataset.outputChars = String(output.length);
    preview.textContent = output.length > 180
      ? `${prefix} (${output.length.toLocaleString()} characters; full text in details): ${output.slice(0, 150)}…`
      : `${tool.error !== undefined || ["failed", "denied"].includes(tool.status) ? "Error" : "Output"}: ${output}`;
  } else if (tool.status !== "pending") {
    preview.textContent = "No tool result was recorded.";
  } else {
    preview.textContent = "Tool result is pending.";
  }
  const omission = toolOmissionSummary(tool.output ?? tool.error);
  if (omission) preview.textContent = `${preview.textContent} · ${omission}`;

  const copyActions = document.createElement("div");
  copyActions.className = "tool-copy-actions";
  if (tool.input !== undefined) {
    copyActions.append(makeCopyButton("Copy tool input", toolLiteralText(tool.input), card, "tool-copy-button"));
  }
  if (tool.output !== undefined) {
    copyActions.append(makeCopyButton("Copy tool result", toolLiteralText(tool.output), card, "tool-copy-button"));
  }
  if (tool.error !== undefined) {
    copyActions.append(makeCopyButton("Copy tool error", toolLiteralText(tool.error), card, "tool-copy-button"));
  }
  const details = document.createElement("div");
  details.className = "tool-detail-body";
  details.id = disclosureId;
  details.hidden = !expanded;
  card.append(button, preview);
  if (copyActions.childElementCount) card.append(copyActions);
  card.append(details);
  if (expanded) appendToolDetails(details, tool);
  button.addEventListener("click", () => {
    const anchor = findScrollAnchor(ui.transcript);
    const previousTop = ui.transcript.scrollTop;
    const open = button.getAttribute("aria-expanded") !== "true";
    button.setAttribute("aria-expanded", String(open));
    button.textContent = `Tool ${tool.name} · ${tool.id || "ID unavailable"} · ${toolStatusLabel(tool.status)} · ${open ? "Hide details" : "Show details"}`;
    details.hidden = !open;
    if (open) {
      state.expandedTools.add(tool.identity);
      details.replaceChildren();
      appendToolDetails(details, tool);
    } else {
      state.expandedTools.delete(tool.identity);
    }
    saveExpandedTools();
    measureTranscriptTurns();
    refreshTranscriptSpacers();
    restoreScrollAnchor(ui.transcript, anchor, previousTop);
  });
  article.append(card);
}

function makeCopyButton(label, text, fallbackContainer, className) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = className;
  button.textContent = label;
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    void copyTranscriptText(text, fallbackContainer);
  });
  return button;
}

async function copyTranscriptText(text, fallbackContainer) {
  try {
    if (!navigator.clipboard || typeof navigator.clipboard.writeText !== "function") {
      throw new Error("clipboard unavailable");
    }
    await navigator.clipboard.writeText(text);
    fallbackContainer.querySelector(".copy-fallback")?.remove();
    ui["copy-status"].textContent = "Copied to clipboard.";
    ui["copy-status"].hidden = false;
  } catch {
    fallbackContainer.querySelector(".copy-fallback")?.remove();
    const fallback = document.createElement("div");
    fallback.className = "copy-fallback";
    const label = document.createElement("label");
    label.htmlFor = `manual-copy-${crypto.randomUUID()}`;
    label.textContent = "Clipboard unavailable or permission denied. Select this text and copy it manually.";
    const textarea = document.createElement("textarea");
    textarea.id = label.htmlFor;
    textarea.readOnly = true;
    textarea.rows = 4;
    textarea.setAttribute("aria-label", "Text to copy manually");
    textarea.value = text;
    fallback.append(label, textarea);
    fallbackContainer.append(fallback);
    textarea.focus();
    textarea.select();
    ui["copy-status"].textContent = "Clipboard copy failed. Select the displayed text and copy it manually.";
    ui["copy-status"].hidden = false;
  }
}

function toolLiteralText(value) {
  if (value === undefined) return "Unavailable";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function appendToolDetails(parent, tool) {
  appendToolField(parent, "Arguments", toolLiteralText(tool.input));
  if (tool.error !== undefined || (["failed", "denied"].includes(tool.status) && tool.output !== undefined)) {
    appendToolField(parent, "Error", toolLiteralText(tool.error ?? tool.output));
  }
  else if (tool.output !== undefined) appendToolField(parent, "Result", toolLiteralText(tool.output));
  else appendToolField(parent, "Result", "No result was recorded.");
  const omission = toolOmissionSummary(tool.output ?? tool.error);
  if (omission) appendToolField(parent, "Output omission", omission);
  appendToolField(parent, "Call observed", formatToolTimestamp(tool.callObservedAt));
  appendToolField(parent, "Result observed", formatToolTimestamp(tool.resultObservedAt));
}

function toolOmissionSummary(value) {
  if (typeof value === "string") {
    try {
      value = JSON.parse(value);
    } catch {
      return "";
    }
  }
  const omissions = [];
  const visit = (node, path, depth) => {
    if (!node || typeof node !== "object" || depth > 5) return;
    for (const [key, nested] of Object.entries(node)) {
      const name = path ? `${path}.${key}` : key;
      if (/_omitted(?:_bytes|_chars)?$/i.test(key) && Number.isFinite(Number(nested)) && Number(nested) > 0) {
        omissions.push(`${name}: ${Number(nested).toLocaleString()} omitted`);
      } else if (nested && typeof nested === "object") {
        visit(nested, name, depth + 1);
      }
    }
  };
  visit(value, "", 0);
  return omissions.join(" · ");
}

function appendToolField(parent, label, value) {
  const field = document.createElement("div");
  field.className = "tool-detail-field";
  const heading = document.createElement("strong");
  heading.textContent = label;
  const literal = document.createElement("pre");
  literal.textContent = value;
  field.append(heading, literal);
  parent.append(field);
}

function formatToolTimestamp(value) {
  const timestamp = Number(value);
  if (!Number.isFinite(timestamp) || timestamp <= 0) return "Timestamp unavailable";
  const formatted = new Date(timestamp).toLocaleString();
  return `${formatted} (${timestamp} ms since Unix epoch)`;
}

function appendTextParagraph(parent, text) {
  const paragraph = document.createElement("p");
  paragraph.textContent = text;
  parent.append(paragraph);
}

function appendMarkdown(parent, source) {
  const sourceText = String(source);
  const codeBlocks = extractFencedCodeBlocks(sourceText);
  let codeBlockIndex = 0;
  const lines = sourceText.replace(/\r\n?/g, "\n").split("\n");
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
        appendCodeBlock(parent, codeBlocks[codeBlockIndex] ?? codeLines.join("\n"));
        codeBlockIndex += 1;
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
    appendCodeBlock(parent, codeBlocks[codeBlockIndex] ?? codeLines.join("\n"));
  }
  flushParagraph();
  flushList();
}

function extractFencedCodeBlocks(source) {
  const blocks = [];
  const pattern = /^[ \t]*```[^\r\n]*(?:\r\n|\r|\n)([\s\S]*?)^[ \t]*```[^\r\n]*(?:\r\n|\r|\n|$)/gm;
  for (const match of source.matchAll(pattern)) blocks.push(match[1]);
  return blocks;
}

function appendCodeBlock(parent, rawCodeText) {
  const wrapper = document.createElement("div");
  wrapper.className = "message-code-block";
  wrapper.append(makeCopyButton("Copy code", rawCodeText, wrapper, "code-copy-button"));
  const pre = document.createElement("pre");
  const code = document.createElement("code");
  code.textContent = rawCodeText;
  pre.append(code);
  wrapper.append(pre);
  parent.append(wrapper);
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

async function submitPrompt({ retry = false, recordedPrompt = null, preserveDraft = false } = {}) {
  if (!state.sessionId || state.submitting || state.active || state.reconnecting) return;
  const sessionId = state.sessionId;
  const switchGeneration = state.switchGeneration;
  if (retry) {
    if (!state.pending?.retryAllowed) return;
  } else {
    const prompt = typeof recordedPrompt === "string" ? recordedPrompt : ui.prompt.value;
    if (!prompt.trim()) return;
    if (state.pending) return;
    if (sessionWorkspaceMissing(state.snapshot?.session)) {
      showSendError("Choose an existing workspace folder for this session before sending.");
      return;
    }
    const requestId = state.snapshot.next_request_id;
    if (!Number.isSafeInteger(requestId)) {
      showSendError("The host cannot provide a new send ID. Reload the conversation before sending.");
      return;
    }
    if (preserveDraft) saveDraft();
    state.preserveDraftRetry = preserveDraft ? { sessionId, requestId } : null;
    state.pending = { request_id: requestId, prompt, phase: "sending", retryAllowed: false, preserveDraft };
    savePending();
  }

  const pending = state.pending;
  if (!pending) return;
  state.submitting = true;
  showSendError("");
  renderStatus();
  try {
    const receipt = await api(`/api/sessions/${encodeURIComponent(sessionId)}/submit`, {
      method: "POST",
      body: { request_id: pending.request_id, prompt: pending.prompt },
    });
    if (!isCurrentSession(sessionId, switchGeneration)) return;
    await acceptSubmission(receipt, pending, { sessionId, switchGeneration });
  } catch (error) {
    if (!isCurrentSession(sessionId, switchGeneration)) return;
    if (error instanceof HostError) {
      if (error.code === "session_busy") {
        state.pending = null;
        savePending();
        saveDraft();
        showSendError(apiErrorText(error.code));
        await refreshSnapshot({ sessionId, switchGeneration }).catch(() => {});
        await refreshSessionList({ quiet: true });
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
      await reconcilePending({ sessionId, switchGeneration });
      if (state.pending) showSendError("The send response was lost. The host status was checked; use Retry same send only if it was not accepted.");
    }
  } finally {
    state.submitting = false;
    renderStatus();
  }
}

async function acceptSubmission(receipt, pending, { sessionId, switchGeneration }) {
  const oldId = sessionId;
  if (!isCurrentSession(oldId, switchGeneration)) return;
  const actualId = receipt.session_id || oldId;
  if (actualId !== oldId) {
    state.streamGeneration += 1;
    state.streamAbort?.abort();
    bindSessionId(oldId, actualId);
  }
  const currentPending = state.pending;
  state.pending = null;
  savePending();
  if (!pending.preserveDraft && ui.prompt.value === pending.prompt) ui.prompt.value = "";
  saveDraft();
  adjustTextarea();
  await api("/api/sessions/select", {
    method: "POST",
    body: { session_id: actualId, tab_id: tabId },
  }).catch(() => {});
  if (!isCurrentSession(actualId, switchGeneration)) return;
  try {
    await refreshSnapshot({ sessionId: actualId, switchGeneration });
    if (!isCurrentSession(actualId, switchGeneration)) return;
    setConnection("Connected to local host.");
  } catch {
    setConnection("Reconnecting…", true);
  }
  connectEvents(state.cursor, actualId, switchGeneration);
  await refreshSessionList({ quiet: true });
  if (!isCurrentSession(actualId, switchGeneration)) return;
  if (receipt.status === "failed" || receipt.status === "incomplete" || receipt.status === "stopped") {
    if (!pending.preserveDraft && !ui.prompt.value.trim()) ui.prompt.value = pending.prompt;
    saveDraft();
    adjustTextarea();
    showSendError(pending.preserveDraft
      ? "The retry did not complete. Your separate composer draft is preserved."
      : "The turn did not complete. Its prompt is kept so you can edit or send it again deliberately.");
  }
  if (currentPending?.request_id !== pending.request_id) return;
}

async function reconcilePending({ sessionId = state.sessionId, switchGeneration = state.switchGeneration } = {}) {
  if (!state.pending || !sessionId || !isCurrentSession(sessionId, switchGeneration)) return;
  const pending = state.pending;
  try {
    const snapshot = await refreshSnapshot({ sessionId, switchGeneration });
    if (!snapshot || !isCurrentSession(sessionId, switchGeneration)) return;
  } catch {
    setConnection("Reconnecting…", true);
    return;
  }
  const snapshot = state.snapshot;
  const receipt = snapshot.last_submission;
  if (Number(snapshot.high_water) >= pending.request_id) {
    if (receipt?.request_id === pending.request_id) {
      const expectedHash = await promptHash(pending.prompt);
      if (!isCurrentSession(sessionId, switchGeneration) || state.pending !== pending) return;
      if (receipt.prompt_sha256 !== expectedHash) {
        pending.phase = "conflict";
        pending.retryAllowed = false;
        savePending();
        showSendError("This send ID was accepted with different text in another tab. Your draft is preserved; inspect the current conversation before sending again.");
        return;
      }
      const actualId = receipt.session_id || snapshot.session.id;
      if (actualId && actualId !== state.sessionId) {
        adoptBoundSession(state.sessionId, actualId, switchGeneration);
      }
      const completedWithError = ["failed", "incomplete", "stopped"].includes(receipt.status);
      state.pending = null;
      savePending();
      if (!pending.preserveDraft && !completedWithError && ui.prompt.value === pending.prompt) ui.prompt.value = "";
      if (!pending.preserveDraft && completedWithError && !ui.prompt.value.trim()) ui.prompt.value = pending.prompt;
      saveDraft();
      adjustTextarea();
      showSendError(completedWithError
        ? pending.preserveDraft
          ? "The retry did not complete. Your separate composer draft is preserved."
          : "The turn did not complete. Its prompt is kept so you can edit or send it again deliberately."
        : "");
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
  if (state.preserveDraftRetry?.sessionId === state.sessionId && state.preserveDraftRetry.requestId === last.request_id) return;
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
  const sessionId = state.sessionId;
  const switchGeneration = state.switchGeneration;
  state.active.status = "stopping";
  renderAll();
  try {
    await api(`/api/sessions/${encodeURIComponent(sessionId)}/stop`, { method: "POST", body: {} });
    await refreshSnapshot({ sessionId, switchGeneration });
    if (!isCurrentSession(sessionId, switchGeneration)) return;
    setConnection("Connected to local host.");
  } catch (error) {
    try {
      await refreshSnapshot({ sessionId, switchGeneration });
      if (!isCurrentSession(sessionId, switchGeneration)) return;
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
  saveCurrentSessionView();
  state.switchGeneration += 1;
  state.streamGeneration += 1;
  state.streamAbort?.abort();
  state.followTranscriptToBottom = false;
  state.sessionId = null;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = null;
  state.expandedTools = new Set();
  state.transcriptItems = [];
  state.transcriptRange = null;
  state.restoreReadingPosition = null;
  state.transcriptSessionId = null;
  ui.messages.replaceChildren();
  ui["empty-transcript"].hidden = false;
  window.sessionStorage.removeItem(sessionKey);
  ui.conversation.hidden = true;
  ui.welcome.hidden = false;
  ui["rename-form"].hidden = true;
  ui["set-workspace-form"].hidden = true;
  ui["workspace-input"].value = window.sessionStorage.getItem(workspaceKey) || "";
  clearStartError();
  renderSessionList();
  setConnection("Connected to local host.");
  ui["workspace-input"].focus();
}

async function renameSession(event) {
  event.preventDefault();
  const sessionId = state.renamingSessionId;
  if (!sessionId) return;
  const name = ui["rename-input"].value.trim();
  if (!name) {
    ui["rename-error"].textContent = "Enter a name for this session.";
    ui["rename-error"].hidden = false;
    ui["rename-input"].focus();
    return;
  }
  try {
    await api(`/api/sessions/${encodeURIComponent(sessionId)}`, { method: "PATCH", body: { name } });
    state.renamingSessionId = null;
    ui["rename-form"].hidden = true;
    await refreshSessionList();
    if (sessionId === state.sessionId) await refreshSnapshot();
  } catch (error) {
    ui["rename-error"].textContent = apiErrorText(error.code || "host_unavailable");
    ui["rename-error"].hidden = false;
  }
}

async function setSessionWorkspace(event) {
  event.preventDefault();
  const sessionId = state.sessionId;
  const switchGeneration = state.switchGeneration;
  if (!sessionId) return;
  const cwd = ui["recovery-workspace-input"].value.trim();
  if (!absoluteWorkspace(cwd)) {
    ui["recovery-error"].textContent = "Enter an absolute path to an existing folder on the host computer.";
    ui["recovery-error"].hidden = false;
    ui["recovery-workspace-input"].focus();
    return;
  }
  try {
    await api(`/api/sessions/${encodeURIComponent(sessionId)}/workspace`, { method: "PUT", body: { cwd } });
    if (!isCurrentSession(sessionId, switchGeneration)) return;
    ui["recovery-error"].hidden = true;
    ui["set-workspace-form"].hidden = true;
    window.sessionStorage.setItem(workspaceKey, cwd);
    await refreshSessionList({ quiet: true });
    await refreshSnapshot({ sessionId, switchGeneration });
  } catch (error) {
    if (!isCurrentSession(sessionId, switchGeneration)) return;
    ui["recovery-error"].textContent = apiErrorText(error.code || "host_unavailable");
    ui["recovery-error"].hidden = false;
  }
}

function formatPromptChange() {
  adjustTextarea();
  saveDraft();
  renderStatus();
}

ui["start-form"].addEventListener("submit", startConversation);
ui["new-conversation"].addEventListener("click", openNewConversation);
ui["session-filter"].addEventListener("input", renderSessionList);
ui["rename-form"].addEventListener("submit", renameSession);
ui["cancel-rename"].addEventListener("click", () => {
  state.renamingSessionId = null;
  ui["rename-form"].hidden = true;
});
ui["set-workspace-form"].addEventListener("submit", setSessionWorkspace);
ui["cancel-workspace"].addEventListener("click", () => {
  ui["set-workspace-form"].hidden = true;
  ui["recovery-error"].hidden = true;
});
ui.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  void submitPrompt();
});
ui.prompt.addEventListener("input", formatPromptChange);
window.addEventListener("resize", adjustTextarea);
ui.prompt.addEventListener("compositionstart", () => { state.composition = true; });
ui.prompt.addEventListener("compositionend", () => { state.composition = false; });
ui.prompt.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || (!event.ctrlKey && !event.metaKey)) return;
  if (state.composition || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  void submitPrompt();
});
ui["retry-submission"].addEventListener("click", () => void submitPrompt({ retry: true }));
ui["transcript-search"].addEventListener("input", () => renderTranscriptSearch({ reset: true, navigate: true }));
ui["transcript-search-prev"].addEventListener("click", () => moveTranscriptSearch(-1));
ui["transcript-search-next"].addEventListener("click", () => moveTranscriptSearch(1));
ui["transcript-search-clear"].addEventListener("click", () => {
  ui["transcript-search"].value = "";
  renderTranscriptSearch();
  ui["transcript-search"].focus();
});
ui["download-transcript"].addEventListener("click", downloadTranscript);
ui["stop-turn"].addEventListener("click", () => void stopTurn());
ui["new-content"].addEventListener("click", () => {
  state.followTranscriptToBottom = true;
  state.restoreReadingPosition = null;
  ui["new-content"].hidden = true;
  ui.transcript.scrollTop = ui.transcript.scrollHeight;
  saveReadingPosition();
  renderTranscript();
  ui.transcript.focus({ preventScroll: true });
});
ui.transcript.addEventListener("pointerdown", () => {
  state.followTranscriptToBottom = false;
});
ui.transcript.addEventListener("wheel", () => {
  state.followTranscriptToBottom = false;
}, { passive: true });
ui.transcript.addEventListener("touchstart", () => {
  state.followTranscriptToBottom = false;
}, { passive: true });
ui.transcript.addEventListener("keydown", (event) => {
  if (event.target !== ui.transcript) return;
  if (event.key === "End") {
    state.followTranscriptToBottom = true;
  } else if (["Home", "PageUp", "PageDown"].includes(event.key)) {
    state.followTranscriptToBottom = false;
  }
  const page = Math.max(120, Math.floor(ui.transcript.clientHeight * 0.8));
  if (event.key === "Home") {
    event.preventDefault();
    ui.transcript.scrollTop = 0;
  } else if (event.key === "End") {
    event.preventDefault();
    ui.transcript.scrollTop = ui.transcript.scrollHeight;
  } else if (event.key === "PageUp") {
    event.preventDefault();
    ui.transcript.scrollTop = Math.max(0, ui.transcript.scrollTop - page);
  } else if (event.key === "PageDown") {
    event.preventDefault();
    ui.transcript.scrollTop += page;
  }
});
ui.transcript.addEventListener("scroll", () => {
  saveReadingPosition();
  scheduleTranscriptRender();
});
window.addEventListener("pagehide", saveCurrentSessionView);
window.addEventListener("online", () => {
  if (state.sessionId) {
    const sessionId = state.sessionId;
    const switchGeneration = state.switchGeneration;
    void refreshSnapshot({ sessionId, switchGeneration }).then((snapshot) => {
      if (snapshot && isCurrentSession(sessionId, switchGeneration)) {
        connectEvents(state.cursor, sessionId, switchGeneration);
      }
    }).catch(() => {});
  }
  void refreshSessionList({ quiet: true });
});

ui["workspace-input"].value = window.sessionStorage.getItem(workspaceKey) || "";
if (!token) {
  setConnection("Open the launch URL to authorize this tab.");
  showStartError("Open the one-time launch URL printed by leg-web. The browser does not store provider credentials.");
} else {
  setConnection("Connecting to local host…");
  void refreshSessionList().then((loaded) => {
    if (state.sessionId) {
      void activateSession(state.sessionId);
    } else if (loaded) {
      setConnection("Connected to local host.");
      ui.welcome.hidden = false;
      ui.conversation.hidden = true;
    } else {
      setConnection("Reconnecting…", true);
      ui.welcome.hidden = false;
      ui.conversation.hidden = true;
    }
  });
  state.listRefreshTimer = window.setInterval(() => void refreshSessionList({ quiet: true }), 2500);
}
