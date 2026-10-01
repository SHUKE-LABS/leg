const tokenKey = "leg-web-launch-token";
const sessionKey = "leg-web-current-session";
const tabKey = "leg-web-tab-id";
const workspaceKey = "leg-web-last-workspace";
const draftPrefix = "leg-web-draft:";
const pendingPrefix = "leg-web-pending:";
const readingPrefix = "leg-web-reading:";

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
    "session-list-error",
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
  restoreReadingPosition: null,
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
  const signature = JSON.stringify({
    selected: state.sessionId,
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
  for (const session of state.sessions) {
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
  const element = [...ui.messages.children].find((node) => node.dataset.key === position.key);
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
  } else if (session.read_only) {
    message = "This saved transcript is read-only. Start a new conversation to continue working.";
  } else if (sessionWorkspaceMissing(session)) {
    message = "Choose a workspace folder before sending a message in this session.";
  } else if (session.run_state === "active" || session.pending_new_turn) {
    message = "This session is busy. Wait for its current turn to finish before sending another message.";
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
  const switchGeneration = ++state.switchGeneration;
  state.streamGeneration += 1;
  state.streamAbort?.abort();
  state.sessionId = sessionId;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = readPending(sessionId);
  state.restoreReadingPosition = readReadingPosition(sessionId) || { atBottom: true };
  window.sessionStorage.setItem(sessionKey, sessionId);
  ui.prompt.value = window.sessionStorage.getItem(storageKey(draftPrefix, sessionId)) || state.pending?.prompt || "";
  adjustTextarea();
  showSendError("");
  ui["rename-form"].hidden = true;
  ui["set-workspace-form"].hidden = true;
  ui.welcome.hidden = true;
  ui.conversation.hidden = false;
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
  if (snapshot.session?.id && snapshot.session.id !== savedId && !state.pending) {
    bindSessionId(savedId, snapshot.session.id);
  }
  state.snapshot = snapshot;
  state.active = snapshot.active;
  state.cursor = Number(snapshot.cursor) || 0;
  renderAll({ eventsArrived });
  return snapshot;
}

function bindSessionId(oldId, actualId) {
  if (!actualId || oldId === actualId) return;
  for (const prefix of [draftPrefix, pendingPrefix, readingPrefix]) {
    const oldKey = storageKey(prefix, oldId);
    const value = window.sessionStorage.getItem(oldKey);
    if (value !== null && window.sessionStorage.getItem(storageKey(prefix, actualId)) === null) {
      window.sessionStorage.setItem(storageKey(prefix, actualId), value);
    }
    window.sessionStorage.removeItem(oldKey);
  }
  state.sessionId = actualId;
  window.sessionStorage.setItem(sessionKey, actualId);
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
    state.snapshot = data;
    state.active = data.active;
    state.cursor = Number(fields.id || data.cursor) || 0;
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
  ui.send.disabled = current.busy || state.reconnecting || state.submitting || Boolean(state.pending) || sessionWorkspaceMissing(session) || Boolean(session?.read_only) || !ui.prompt.value.trim();
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
  renderSessionList();
  if (!hasSession) {
    renderStatus();
    return;
  }
  const session = state.snapshot.session;
  ui["session-title"].dataset.sessionId = session.id;
  ui["session-title"].textContent = session.name || "Conversation";
  const cwd = session.cwd || "Workspace not set";
  ui["workspace-label"].textContent = `Workspace: ${cwd}`;
  ui["workspace-warning"].hidden = Boolean((session.turns || []).length || state.snapshot.high_water > 0);
  renderSessionGuidance();
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

  if (state.restoreReadingPosition) {
    restoreReadingPosition(scroller, state.restoreReadingPosition);
    state.restoreReadingPosition = null;
  } else if (wasAtBottom) {
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
  const sessionId = state.sessionId;
  const switchGeneration = state.switchGeneration;
  if (retry) {
    if (!state.pending?.retryAllowed) return;
  } else {
    const prompt = ui.prompt.value;
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
    state.pending = { request_id: requestId, prompt, phase: "sending", retryAllowed: false };
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
  if (ui.prompt.value === pending.prompt) ui.prompt.value = "";
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
    if (!ui.prompt.value.trim()) ui.prompt.value = pending.prompt;
    saveDraft();
    adjustTextarea();
    showSendError("The turn did not complete. Its prompt is kept so you can edit or send it again deliberately.");
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
        bindSessionId(state.sessionId, actualId);
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
  state.sessionId = null;
  state.snapshot = null;
  state.active = null;
  state.cursor = 0;
  state.pending = null;
  state.restoreReadingPosition = null;
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
ui.transcript.addEventListener("scroll", saveReadingPosition);
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
