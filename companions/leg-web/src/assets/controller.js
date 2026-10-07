export function startController(view) {
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

  const features = view.features;

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
    connectionText: "",
    submitting: false,
    startBusy: false,
    renamingSessionId: null,
    renameDraft: "",
    renameError: "",
    showRecoveryForm: false,
    recoveryWorkspaceDraft: "",
    recoveryError: "",
    setupError: "",
    sendError: "",
    sessionListError: "",
    workspaceDraft: window.sessionStorage.getItem(workspaceKey) || "",
    draft: "",
    loadingSession: false,
    transcriptItems: [],
    preserveDraftRetry: null,
  };

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

  function sessionWorkspaceMissing(session) {
    return !session?.cwd || (session.warnings || []).some((warning) => /recorded workspace .* is missing/i.test(warning));
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
      state.sessionListError = "";
      publishThemeModel();
      return true;
    } catch (error) {
      if (!quiet) {
        state.sessionListError = apiErrorText(error.code || "host_unavailable");
        publishThemeModel();
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

  function saveCurrentSessionView() {
    if (!state.sessionId) return;
    saveDraft();
    savePending();
    view.save();
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
    state.connectionText = text;
    publishThemeModel();
  }

  function showStartError(message) {
    state.setupError = message;
    publishThemeModel();
  }

  function clearStartError() {
    state.setupError = "";
    publishThemeModel();
  }

  function showSendError(message) {
    state.sendError = message;
    publishThemeModel();
  }

  function saveDraft() {
    if (!state.sessionId) return;
    window.sessionStorage.setItem(storageKey(draftPrefix, state.sessionId), state.draft);
  }

  function savePending() {
    if (!state.sessionId) return;
    if (state.pending) {
      window.sessionStorage.setItem(storageKey(pendingPrefix, state.sessionId), JSON.stringify(state.pending));
    } else {
      window.sessionStorage.removeItem(storageKey(pendingPrefix, state.sessionId));
    }
  }

  function absoluteWorkspace(value) {
    return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value);
  }

  async function startConversation(workspace) {
    if (state.startBusy) return;
    const switchGeneration = state.switchGeneration;
    const cwd = String(workspace ?? state.workspaceDraft).trim();
    state.workspaceDraft = cwd;
    if (!absoluteWorkspace(cwd)) {
      showStartError("Enter an absolute path, such as /home/name/project or C:\\Users\\name\\project.");
      view.focus("workspace");
      return;
    }
    state.startBusy = true;
    clearStartError();
    try {
      const session = await api("/api/sessions", { method: "POST", body: { cwd } });
      window.sessionStorage.setItem(workspaceKey, cwd);
      state.workspaceDraft = cwd;
      await refreshSessionList({ quiet: true });
      if (state.switchGeneration === switchGeneration) await activateSession(session.id);
    } catch (error) {
      showStartError(apiErrorText(error.code || "host_unavailable"));
    } finally {
      state.startBusy = false;
      publishThemeModel();
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
    state.transcriptItems = [];
    state.loadingSession = true;
    state.renamingSessionId = null;
    state.renameDraft = "";
    state.renameError = "";
    state.showRecoveryForm = false;
    state.recoveryError = "";
    window.sessionStorage.setItem(sessionKey, sessionId);
    const savedDraft = window.sessionStorage.getItem(storageKey(draftPrefix, sessionId));
    state.draft = savedDraft !== null
      ? savedDraft
      : state.pending?.preserveDraft ? "" : state.pending?.prompt || "";
    state.sendError = "";
    setConnection("Opening conversation…");
    try {
      await api("/api/sessions/select", {
        method: "POST",
        body: { session_id: sessionId, tab_id: tabId },
      });
      const snapshot = await refreshSnapshot({ sessionId, switchGeneration });
      if (!snapshot || state.switchGeneration !== switchGeneration) return;
      state.loadingSession = false;
      renderAll();
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
        state.loadingSession = false;
        setConnection("Conversation unavailable.");
        showStartError("This conversation is no longer available. Start a new conversation to continue.");
        return;
      }
      setConnection("Reconnecting…", true);
      state.loadingSession = false;
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
    for (const prefix of [draftPrefix, pendingPrefix]) {
      const oldKey = storageKey(prefix, oldId);
      const value = window.sessionStorage.getItem(oldKey);
      if (value !== null && window.sessionStorage.getItem(storageKey(prefix, actualId)) === null) {
        window.sessionStorage.setItem(storageKey(prefix, actualId), value);
      }
      window.sessionStorage.removeItem(oldKey);
    }
    view.rebindSessionId(oldId, actualId);
    state.sessionId = actualId;
    if (state.snapshot?.session?.id === oldId) state.snapshot.session.id = actualId;
    window.sessionStorage.setItem(sessionKey, actualId);
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

  function readOnly(value, cache = new WeakMap()) {
    if (!value || typeof value !== "object") return value;
    if (cache.has(value)) return cache.get(value);
    const proxy = new Proxy(value, {
      get(target, key, receiver) {
        return readOnly(Reflect.get(target, key, receiver), cache);
      },
      set() { throw new TypeError("Theme view models are read-only."); },
      deleteProperty() { throw new TypeError("Theme view models are read-only."); },
      defineProperty() { throw new TypeError("Theme view models are read-only."); },
    });
    cache.set(value, proxy);
    return proxy;
  }

  function publishThemeModel(renderOptions = {}) {
    if (typeof view?.render !== "function" || typeof viewActions === "undefined") return;
    const status = currentStatus();
    const session = state.snapshot?.session;
    const retryableTurnIndices = (session?.turns || [])
      .filter((turn) => canRetryTurn(turn))
      .map((turn) => Number(turn.turn_index));
    const model = readOnly({
      selectedSessionId: state.sessionId,
      sessions: state.sessions,
      session,
      snapshot: state.snapshot,
      transcript: state.transcriptItems,
      active: state.active,
      pending: state.pending,
      draft: state.draft,
      workspaceDraft: state.workspaceDraft,
      recoveryWorkspaceDraft: state.recoveryWorkspaceDraft,
      renameDraft: state.renameDraft,
      submitting: state.submitting,
      startBusy: state.startBusy,
      renamingSessionId: state.renamingSessionId,
      showRecoveryForm: state.showRecoveryForm,
      loadingSession: state.loadingSession,
      errors: {
        sessionList: state.sessionListError,
        setup: state.setupError,
        send: state.sendError,
        rename: state.renameError,
        recovery: state.recoveryError,
      },
      status,
      availability: {
        send: !status.busy && !state.reconnecting && !state.submitting && !state.pending &&
          !sessionWorkspaceMissing(session) && !session?.read_only,
        stop: Boolean(state.active && state.active.status !== "stopping" && !state.reconnecting),
        retrySameSend: Boolean(state.pending?.retryAllowed && !status.busy && !state.submitting),
        createSession: !status.busy && !state.reconnecting && !state.submitting && !state.pending,
      },
      connection: { text: state.connectionText, reconnecting: state.reconnecting },
      retryableTurnIndices,
      renderOptions,
      features,
    });
    view.render(model, viewActions);
  }

  function renderStatus() {
    publishThemeModel();
  }

  function renderAll(options = {}) {
    state.transcriptItems = buildTranscriptItems();
    publishThemeModel(options);
  }

  function renderSessionList() {
    publishThemeModel();
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
      const prompt = typeof recordedPrompt === "string" ? recordedPrompt : state.draft;
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
    if (!pending.preserveDraft && state.draft === pending.prompt) state.draft = "";
    saveDraft();
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
      if (!pending.preserveDraft && !state.draft.trim()) state.draft = pending.prompt;
      saveDraft();
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
        if (!pending.preserveDraft && !completedWithError && state.draft === pending.prompt) state.draft = "";
        if (!pending.preserveDraft && completedWithError && !state.draft.trim()) state.draft = pending.prompt;
        saveDraft();
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
    if (!state.snapshot || state.draft.trim()) return;
    const last = state.snapshot.last_submission;
    if (!last || !["failed", "incomplete", "stopped"].includes(last.status)) return;
    if (state.preserveDraftRetry?.sessionId === state.sessionId && state.preserveDraftRetry.requestId === last.request_id) return;
    const turns = state.snapshot.session?.turns || [];
    const turn = turns[turns.length - 1];
    if (turn?.prompt) {
      state.draft = turn.prompt;
      saveDraft();
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
    state.transcriptItems = [];
    window.sessionStorage.removeItem(sessionKey);
    state.renamingSessionId = null;
    state.renameDraft = "";
    state.renameError = "";
    state.showRecoveryForm = false;
    state.recoveryError = "";
    state.workspaceDraft = window.sessionStorage.getItem(workspaceKey) || "";
    state.draft = "";
    state.setupError = "";
    setConnection("Connected to local host.");
    view.focus("workspace");
  }

  async function renameSession(value) {
    const sessionId = state.renamingSessionId;
    if (!sessionId) return;
    const name = String(value ?? state.renameDraft).trim();
    state.renameDraft = name;
    if (!name) {
      state.renameError = "Enter a name for this session.";
      publishThemeModel();
      view.focus("rename");
      return;
    }
    try {
      await api(`/api/sessions/${encodeURIComponent(sessionId)}`, { method: "PATCH", body: { name } });
      state.renamingSessionId = null;
      state.renameError = "";
      await refreshSessionList();
      if (sessionId === state.sessionId) await refreshSnapshot();
    } catch (error) {
      state.renameError = apiErrorText(error.code || "host_unavailable");
      publishThemeModel();
    }
  }

  async function setSessionWorkspace(value) {
    const sessionId = state.sessionId;
    const switchGeneration = state.switchGeneration;
    if (!sessionId) return;
    const cwd = String(value ?? state.recoveryWorkspaceDraft).trim();
    state.recoveryWorkspaceDraft = cwd;
    if (!absoluteWorkspace(cwd)) {
      state.recoveryError = "Enter an absolute path to an existing folder on the host computer.";
      publishThemeModel();
      view.focus("recovery");
      return;
    }
    try {
      await api(`/api/sessions/${encodeURIComponent(sessionId)}/workspace`, { method: "PUT", body: { cwd } });
      if (!isCurrentSession(sessionId, switchGeneration)) return;
      state.recoveryError = "";
      state.showRecoveryForm = false;
      state.recoveryWorkspaceDraft = "";
      window.sessionStorage.setItem(workspaceKey, cwd);
      state.workspaceDraft = cwd;
      await refreshSessionList({ quiet: true });
      await refreshSnapshot({ sessionId, switchGeneration });
    } catch (error) {
      if (!isCurrentSession(sessionId, switchGeneration)) return;
      state.recoveryError = apiErrorText(error.code || "host_unavailable");
      publishThemeModel();
    }
  }

  function updateDraft(value) {
    state.draft = String(value ?? "");
    saveDraft();
    renderStatus();
  }

  const viewActions = Object.freeze({
    startConversation: (cwd) => void startConversation(cwd),
    openNewConversation,
    selectSession: (sessionId) => void activateSession(sessionId),
    beginRename(sessionId) {
      const session = state.sessions.find((item) => item.id === sessionId);
      if (!session) return;
      state.renamingSessionId = sessionId;
      state.renameDraft = session.name || "";
      state.renameError = "";
      publishThemeModel();
      view.focus("rename");
    },
    updateRenameDraft(value) { state.renameDraft = String(value ?? ""); },
    updateWorkspaceDraft(value) { state.workspaceDraft = String(value ?? ""); },
    updateRecoveryWorkspaceDraft(value) { state.recoveryWorkspaceDraft = String(value ?? ""); },
    renameSession: (value) => void renameSession(value),
    cancelRename() {
      state.renamingSessionId = null;
      state.renameDraft = "";
      state.renameError = "";
      publishThemeModel();
    },
    setSessionWorkspace: (cwd) => void setSessionWorkspace(cwd),
    cancelWorkspace() {
      state.showRecoveryForm = false;
      state.recoveryError = "";
      publishThemeModel();
    },
    showWorkspaceForm() {
      state.showRecoveryForm = true;
      state.recoveryError = "";
      publishThemeModel();
      view.focus("recovery");
    },
    submitPrompt: () => void submitPrompt(),
    updateDraft,
    retrySubmission: () => void submitPrompt({ retry: true }),
    stopTurn: () => void stopTurn(),
    retryRecordedTurn(turnIndex) {
      const turn = state.snapshot?.session?.turns?.find((item) => Number(item.turn_index) === Number(turnIndex));
      if (turn && canRetryTurn(turn)) void submitPrompt({ recordedPrompt: turn.prompt, preserveDraft: true });
    },
    saveCurrentSessionView,
    online() {
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
    },
  });
  view.bind(viewActions);

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
      } else {
        setConnection("Reconnecting…", true);
      }
    });
    state.listRefreshTimer = window.setInterval(() => void refreshSessionList({ quiet: true }), 2500);
  }
  return saveCurrentSessionView;
}
