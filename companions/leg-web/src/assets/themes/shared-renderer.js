const INITIAL_TURN_HEIGHT = 280;
const TURN_OVERSCAN = 2;
const readingPrefix = "leg-web-reading:";
const inspectionPrefix = "leg-web-inspection:";
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

export function createSharedRenderer(ui, features) {
  const requiredElements = [
    "connection-state", "new-conversation", "session-list", "session-list-empty",
    "session-list-no-results", "session-list-error", "rename-form", "rename-input",
    "rename-error", "cancel-rename", "welcome", "start-form", "workspace-input",
    "setup-error", "conversation", "session-title", "workspace-label", "session-guidance",
    "set-workspace-form", "recovery-workspace-input", "recovery-error", "cancel-workspace",
    "provider-model", "elapsed-time", "turn-status", "active-tool", "stop-turn",
    "workspace-warning", "connection-message", "transcript-warnings", "transcript",
    "messages", "empty-transcript", "new-content", "composer", "prompt",
    "retry-submission", "send-error", "live-status",
  ];
  const featureElements = {
    titleFilter: ["session-filter"],
    transcriptSearch: [
      "open-transcript-search", "close-transcript-search", "transcript-find-bar",
      "transcript-search", "transcript-search-prev", "transcript-search-next",
      "transcript-search-clear", "transcript-search-status",
    ],
    copy: ["copy-status"],
    download: ["download-transcript", "download-status"],
  };
  const missing = requiredElements.filter((key) => !ui[key]);
  for (const [feature, keys] of Object.entries(featureElements)) {
    if (features[feature]) missing.push(...keys.filter((key) => !ui[key]));
  }
  if (missing.length) throw new Error(`theme_contract_missing:${missing.join(",")}`);
  const state = {
    sessionId: null, sessions: [], snapshot: null, active: null, pending: null,
    transcriptItems: [], transcriptHeights: new Map(), transcriptAverageHeight: INITIAL_TURN_HEIGHT,
    transcriptRange: null, transcriptRenderQueued: false, followTranscriptToBottom: false,
    transcriptSearchMatches: [], transcriptSearchIndex: -1, expandedTools: new Set(),
    restoreReadingPosition: null, transcriptSessionId: null, renderedSessionSignature: "",
    connectionText: "", reconnecting: false, submitting: false, startBusy: false,
    renamingSessionId: null, draft: "", workspaceDraft: "", recoveryWorkspaceDraft: "",
    renameDraft: "", sessionListError: "", setupError: "", sendError: "",
    renameError: "", recoveryError: "", showRecoveryForm: false, loadingSession: false,
    sessionListFilter: "", status: { text: "Idle", busy: false },
    availability: {}, retryableTurnIndices: [], renderOptions: {}, lastAnnouncedStatus: "",
  };
  let actions = {};
  let elapsedTimer = null;
  const messageRenderState = new WeakMap();
  const toastTimers = new WeakMap();

  function storageKey(prefix, sessionId) { return `${prefix}${sessionId}`; }
  function readExpandedTools(sessionId) {
    try {
      const saved = window.sessionStorage.getItem(storageKey(inspectionPrefix, sessionId));
      const keys = JSON.parse(saved || "[]");
      return new Set(Array.isArray(keys) ? keys.filter((key) => typeof key === "string") : []);
    } catch { return new Set(); }
  }
  function saveExpandedTools() {
    if (!state.sessionId) return;
    window.sessionStorage.setItem(storageKey(inspectionPrefix, state.sessionId), JSON.stringify([...state.expandedTools]));
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


  function renderSessionList() {
    const filter = (ui["session-filter"]?.value || "").trim().toLocaleLowerCase();
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


  function renderSessionGuidance() {
    const session = state.snapshot?.session;
    ui["session-guidance"].replaceChildren();
    ui["session-guidance"].hidden = true;
    ui["set-workspace-form"].hidden = !state.showRecoveryForm;
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
      choose.dataset.action = "show-workspace";
      choose.className = "quiet-button";
      choose.textContent = "Set workspace";
      ui["session-guidance"].append(choose);
    }
    ui["session-guidance"].hidden = false;
  }

  function adjustTextarea() {
    ui.prompt.style.height = "auto";
    const maxHeight = window.innerHeight * 0.4;
    const contentHeight = ui.prompt.scrollHeight;
    ui.prompt.style.height = `${Math.min(contentHeight, maxHeight)}px`;
    ui.prompt.style.overflowY = contentHeight > maxHeight ? "auto" : "hidden";
  }

  function renderStatus() {
    const current = state.status;
    const activeTool = state.active?.active_tool;
    ui["turn-status"].textContent = current.text;
    ui["active-tool"].textContent = activeTool ? `Active tool: ${activeTool.tool_name || "tool"}` : "";
    ui["active-tool"].hidden = !activeTool;
    ui["stop-turn"].hidden = !state.active;
    ui["stop-turn"].disabled = !state.availability.stop;
    if (features.download) ui["download-transcript"].disabled = !state.snapshot;
    if (ui.send) ui.send.disabled = !state.availability.send || !state.draft.trim();
    ui["new-conversation"].disabled = !state.availability.createSession;
    const canRetry = Boolean(state.availability.retrySameSend);
    ui["retry-submission"].hidden = !canRetry;
    ui["retry-submission"].disabled = !canRetry;
    for (const button of ui.messages.querySelectorAll(".retry-turn-button")) {
      button.disabled = !state.retryableTurnIndices.includes(Number(button.dataset.turnIndex));
    }
    const statusKey = `${current.text}:${activeTool?.tool_name || ""}`;
    if (statusKey !== state.lastAnnouncedStatus) {
      state.lastAnnouncedStatus = statusKey;
      ui["live-status"].textContent = activeTool
        ? `${current.text}. Active tool: ${activeTool.tool_name || "tool"}.`
        : current.text;
    }
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
      ui["elapsed-time"].textContent = `Elapsed ${Math.max(0, Math.floor((Date.now() - started) / 1000))}s`;
    };
    update();
    if (!elapsedTimer) elapsedTimer = window.setInterval(update, 1000);
  }

  function renderAll(options = {}) {
    const session = state.snapshot?.session;
    const hasSession = Boolean(state.sessionId && (session || state.loadingSession));
    ui.welcome.hidden = hasSession;
    ui.conversation.hidden = !hasSession;
    ui["session-list-error"].textContent = state.sessionListError;
    ui["session-list-error"].hidden = !state.sessionListError;
    ui["setup-error"].textContent = state.setupError;
    ui["setup-error"].hidden = !state.setupError;
    ui["send-error"].textContent = state.sendError;
    ui["send-error"].hidden = !state.sendError;
    ui["rename-error"].textContent = state.renameError;
    ui["rename-error"].hidden = !state.renameError;
    ui["rename-form"].hidden = !state.renamingSessionId;
    ui["recovery-error"].textContent = state.recoveryError;
    ui["recovery-error"].hidden = !state.recoveryError;
    ui["set-workspace-form"].hidden = !state.showRecoveryForm;
    ui["connection-state"].textContent = state.connectionText === "Connected to local host." ? "" : state.connectionText;
    ui["connection-message"].hidden = !state.reconnecting;
    ui["connection-message"].textContent = state.reconnecting
      ? "Reconnecting to the local host. The draft is kept, and no message will be resent."
      : "";
    ui["start-form"].querySelector("button[type=submit]").disabled = state.startBusy;
    renderSessionList();
    if (!hasSession) {
      ui.messages.replaceChildren();
      ui["empty-transcript"].hidden = false;
      ui["empty-transcript"].textContent = "Your conversation will appear here.";
      renderElapsed();
      renderStatus();
      return;
    }
    if (session) ui["session-title"].dataset.sessionId = session.id;
    ui["session-title"].textContent = session ? sessionName(session) : "Opening conversation…";
    ui["empty-transcript"].textContent = state.loadingSession ? "Opening conversation…" : "Your conversation will appear here.";
    ui["empty-transcript"].hidden = state.transcriptItems.length > 0;
    const cwd = session?.cwd || "Workspace not set";
    ui["workspace-label"].textContent = cwd;
    ui["workspace-label"].title = cwd;
    ui["workspace-warning"].hidden = Boolean((session?.turns || []).length || state.snapshot?.high_water > 0);
    renderSessionGuidance();
    renderProviderModel();
    renderElapsed();
    renderStatus();
    renderTranscriptWarnings();
    renderTranscript(options);
  }

  function showNewContent() {
    state.followTranscriptToBottom = true;
    state.restoreReadingPosition = null;
    ui.transcript.scrollTop = ui.transcript.scrollHeight;
    saveReadingPosition();
    renderTranscript();
    ui.transcript.focus({ preventScroll: true });
  }

  function transcriptKeydown(event) {
    if (event.target !== ui.transcript) return;
    if (event.key === "End") state.followTranscriptToBottom = true;
    else if (["Home", "PageUp", "PageDown"].includes(event.key)) state.followTranscriptToBottom = false;
    const page = Math.max(120, Math.floor(ui.transcript.clientHeight * 0.8));
    if (event.key === "Home") { event.preventDefault(); ui.transcript.scrollTop = 0; }
    else if (event.key === "End") { event.preventDefault(); ui.transcript.scrollTop = ui.transcript.scrollHeight; }
    else if (event.key === "PageUp") { event.preventDefault(); ui.transcript.scrollTop = Math.max(0, ui.transcript.scrollTop - page); }
    else if (event.key === "PageDown") { event.preventDefault(); ui.transcript.scrollTop += page; }
  }

  function conversationKeydown(event) {
    if (event.key === "Escape" && !ui["transcript-find-bar"].hidden) {
      event.preventDefault(); closeTranscriptSearch(); return;
    }
    if (!(event.ctrlKey || event.metaKey) || !event.shiftKey || event.altKey || event.key.toLowerCase() !== "f") return;
    event.preventDefault(); openTranscriptSearch();
  }

  function searchKeydown(event) {
    if (event.target === ui["transcript-search"] && event.key === "Enter") {
      if (event.isComposing || event.keyCode === 229) return;
      event.preventDefault(); moveTranscriptSearch(event.shiftKey ? -1 : 1);
    }
  }

  function clearSearch() {
    ui["transcript-search"].value = "";
    renderTranscriptSearch();
    ui["transcript-search"].focus();
  }


  function renderTranscript({ eventsArrived = false, force = true } = {}) {
    if (!state.snapshot) return;
    const scroller = ui.transcript;
    const items = state.transcriptItems;
    const previousTop = scroller.scrollTop;
    const previousBottomGap = scroller.scrollHeight - scroller.clientHeight - previousTop;
    const wasAtBottom = state.followTranscriptToBottom || previousBottomGap < 36;
    const anchor = findScrollAnchor(scroller);
    const restore = state.restoreReadingPosition;

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
      for (const round of item.toolRounds || []) {
        for (const block of round.content || []) {
          if (block?.type === "text") add(item.key, item.assistantKey, "Tool round text", block.text);
        }
      }
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
    if (!features.transcriptSearch) return;
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
    ui["transcript-search-status"].textContent = `${state.transcriptSearchIndex + 1}/${count} · ${match.label}: ${excerpt}`;

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
        : turn.querySelector(`.tool-inspector[data-tool-index="${match.toolIndex}"], .tool-summary[data-tool-index="${match.toolIndex}"]`) || turn;
      const group = target.closest(".tool-call-group");
      const groupDisclosure = group?.querySelector(".tool-group-disclosure");
      if (groupDisclosure?.getAttribute("aria-expanded") !== "true") groupDisclosure?.click();
      const toolDisclosure = target.querySelector(".tool-disclosure");
      if (toolDisclosure?.getAttribute("aria-expanded") !== "true") toolDisclosure?.click();
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

  function openTranscriptSearch() {
    ui["transcript-find-bar"].hidden = false;
    ui["open-transcript-search"].setAttribute("aria-expanded", "true");
    ui["transcript-search"].focus({ preventScroll: true });
    ui["transcript-search"].select();
  }

  function closeTranscriptSearch() {
    ui["transcript-search"].value = "";
    renderTranscriptSearch({ reset: true });
    ui["transcript-find-bar"].hidden = true;
    ui["open-transcript-search"].setAttribute("aria-expanded", "false");
    ui["open-transcript-search"].focus({ preventScroll: true });
  }

  function showTransientToast(element, message) {
    clearTransientToast(element);
    element.textContent = message;
    element.hidden = false;
    toastTimers.set(element, window.setTimeout(() => {
      element.hidden = true;
      element.textContent = "";
      toastTimers.delete(element);
    }, 3500));
  }

  function clearTransientToast(element) {
    const timer = toastTimers.get(element);
    if (timer) window.clearTimeout(timer);
    toastTimers.delete(element);
    element.hidden = true;
    element.textContent = "";
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
    if (features.download) showTransientToast(ui["download-status"], "Transcript download started.");
    } catch {
      if (features.download) showTransientToast(ui["download-status"], "Transcript download could not be created.");
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
    if (features.copy) {
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
    }
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
    const signature = JSON.stringify([item.tools, item.toolRounds, status, item.capped, item.outcome, Boolean(item.active), canRetryTurn(sourceTurn)]);
    const rendered = messageRenderState.get(article);
    if (rendered.details !== signature) {
      rendered.details = signature;
      for (const detail of article.querySelectorAll(".tool-round-text, .tool-summary, .tool-inspector, .tool-call-group, .turn-outcome, .turn-warning, .retry-turn-control")) {
        detail.remove();
      }
      for (const round of item.toolRounds || []) {
        for (const [blockIndex, block] of (round.content || []).entries()) {
          if (block?.type !== "text" || typeof block.text !== "string" || !block.text.trim()) continue;
          const text = document.createElement("div");
          text.className = "tool-round-text";
          text.dataset.roundIndex = String(round.roundIndex);
          text.dataset.blockIndex = String(blockIndex);
          text.textContent = block.text;
          article.append(text);
        }
      }
      for (const [toolIndex, tool] of item.tools.entries()) {
        if (features.toolInspection) appendToolInspector(article, tool, toolIndex);
        else appendToolSummary(article, tool, toolIndex);
      }
      groupFinishedToolRows(article, item);
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
    arrangeMessageDetails(article, item);
  }

  function arrangeMessageDetails(article, item) {
    const heading = article.querySelector(".message-heading");
    const body = article.querySelector(".message-content");
    if (!heading || !body) return;

    const toolNodes = [...article.querySelectorAll(".tool-inspector, .tool-summary")];
    const toolContainer = (node) => node.closest(".tool-call-group") || node;
    const toolById = new Map(toolNodes
      .filter((node) => node.dataset.toolUseId)
      .map((node) => [node.dataset.toolUseId, toolContainer(node)]));
    const toolByIndex = new Map(toolNodes.map((node) => [Number(node.dataset.toolIndex), toolContainer(node)]));
    const textByKey = new Map([...article.querySelectorAll(".tool-round-text")]
      .map((node) => [`${node.dataset.roundIndex}:${node.dataset.blockIndex}`, node]));
    const ordered = [heading];
    const included = new Set(ordered);

    const add = (node) => {
      if (node && !included.has(node)) {
        ordered.push(node);
        included.add(node);
      }
    };
    const addTool = (id, toolIndex = null) => add(
      (id && toolById.get(String(id))) || (toolIndex === null ? null : toolByIndex.get(toolIndex)),
    );

    if ((item.toolRounds || []).length) {
      for (const round of item.toolRounds) {
        for (const [blockIndex, block] of (round.content || []).entries()) {
          if (block?.type === "text") add(textByKey.get(`${round.roundIndex}:${blockIndex}`));
          if (block?.type === "tool_use") addTool(block.id);
        }
      }
      for (const [toolIndex, tool] of (item.tools || []).entries()) addTool(tool.id, toolIndex);
      add(body);
    } else {
      add(body);
      for (const [toolIndex, tool] of (item.tools || []).entries()) addTool(tool.id, toolIndex);
    }

    for (const node of article.querySelectorAll(".turn-outcome, .turn-warning, .retry-turn-control")) add(node);
    for (const node of [...article.children]) add(node);

    let current = article.firstElementChild;
    for (const node of ordered) {
      if (node === current) current = current.nextElementSibling;
      else article.insertBefore(node, current);
    }
  }

  function appendOutcome(article, text) {
    const status = document.createElement("p");
    status.className = `turn-outcome turn-outcome-${text.toLowerCase()}`;
    status.textContent = text;
    article.append(status);
  }

  function canRetryTurn(turn) {
    return Boolean(turn && state.retryableTurnIndices.includes(Number(turn.turn_index)));
  }

  function appendRetryTurnControl(article, turn) {
    const control = document.createElement("div");
    control.className = "retry-turn-control";
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "retry-turn-button";
    retry.dataset.action = "retry-recorded-turn";
    retry.dataset.turnIndex = String(turn.turn_index);
    retry.textContent = "Retry turn";
    retry.setAttribute("aria-label", `Retry turn ${turn.turn_index}`);
    retry.title = "Resubmits this prompt; tool side effects may repeat";
    retry.disabled = !canRetryTurn(turn);
    control.append(retry);
    article.append(control);
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

  function toolInputRecord(tool) {
    let input = tool.input;
    if (typeof input === "string") {
      try { input = JSON.parse(input); } catch { return {}; }
    }
    return input && typeof input === "object" && !Array.isArray(input) ? input : {};
  }

  function bashResultEnvelope(tool, value = tool.error ?? tool.output) {
    if (tool.name !== "bash" || value === undefined) return null;
    let envelope = value;
    if (typeof envelope === "string") {
      try { envelope = JSON.parse(envelope); } catch { return null; }
    }
    if (!envelope || typeof envelope !== "object" || Array.isArray(envelope)) return null;
    return ["stdout", "stderr", "exit_code", "stdout_omitted_bytes", "stderr_omitted_bytes"]
      .some((key) => Object.hasOwn(envelope, key)) ? envelope : null;
  }

  function toolOutputPresentation(tool, value = tool.error ?? tool.output) {
    const envelope = bashResultEnvelope(tool, value);
    if (!envelope) return { text: toolLiteralText(value), envelope: null, exitCode: null };
    const stdout = envelope.stdout === undefined ? "" : String(envelope.stdout);
    const stderr = envelope.stderr === undefined ? "" : String(envelope.stderr);
    const text = stdout && stderr && !stdout.endsWith("\n")
      ? `${stdout}\n${stderr}`
      : `${stdout}${stderr}`;
    const exitCode = envelope.exit_code === null || envelope.exit_code === undefined || envelope.exit_code === ""
      ? null
      : Number(envelope.exit_code);
    return {
      text,
      envelope,
      exitCode: Number.isFinite(exitCode) ? exitCode : null,
    };
  }

  function toolDisplayStatus(tool, output = toolOutputPresentation(tool)) {
    return output.exitCode !== null && output.exitCode !== 0 ? "failed" : tool.status;
  }

  function toolInputSummary(tool) {
    const input = toolInputRecord(tool);
    if (tool.name === "bash") {
      const summary = typeof input.description === "string" && input.description.trim()
        ? input.description
        : input.command;
      return oneLineToolSummary(summary);
    }
    if (tool.name === "read") {
      const parts = [typeof input.path === "string" ? input.path : "Path unavailable"];
      if (input.offset !== undefined) parts.push(`offset ${input.offset}`);
      if (input.limit !== undefined) parts.push(`limit ${input.limit}`);
      return parts.join(" · ");
    }
    const firstString = typeof tool.input === "string"
      ? tool.input
      : Object.values(input).find((value) => typeof value === "string");
    return oneLineToolSummary(firstString ?? tool.input);
  }

  function oneLineToolSummary(value) {
    const summary = typeof value === "string" ? value : value === undefined ? "Input unavailable" : toolLiteralText(value);
    return summary.replace(/\s+/g, " ").trim() || "Input unavailable";
  }

  function toolStatusGlyph(status) {
    if (status === "pending") return "⟳";
    if (status === "completed") return "✓";
    if (status === "failed" || status === "denied") return "✗";
    if (status === "interrupted") return "Ⅱ";
    if (status === "missing" || status === "unavailable") return "?";
    return "·";
  }

  function setToolDisclosureLabel(button, tool, expanded) {
    const output = toolOutputPresentation(tool);
    const status = toolDisplayStatus(tool, output);
    const statusLabel = toolStatusLabel(status);
    const summary = toolInputSummary(tool);
    const exitCode = output.exitCode;
    const glyph = document.createElement("span");
    glyph.className = `tool-status-glyph tool-status-glyph-${status}`;
    glyph.setAttribute("aria-hidden", "true");
    glyph.textContent = toolStatusGlyph(status);
    const name = document.createElement("span");
    name.className = "tool-row-name";
    name.textContent = tool.name || "tool";
    const input = document.createElement("span");
    input.className = "tool-row-summary";
    input.textContent = summary;
    button.replaceChildren(glyph, name, input);
    if (exitCode !== null && exitCode !== 0) {
      const exit = document.createElement("span");
      exit.className = "tool-exit-code";
      exit.textContent = `exit ${exitCode}`;
      button.append(exit);
    }
    const action = expanded ? "Hide details" : "Show details";
    button.setAttribute("aria-label", `${statusLabel} ${tool.name || "tool"}: ${summary}${exitCode !== null && exitCode !== 0 ? `, exit ${exitCode}` : ""}. ${action}`);
  }

  function renderToolDetails(parent, tool) {
    parent.replaceChildren();
    appendToolDetails(parent, tool);
    const copyActions = document.createElement("div");
    copyActions.className = "tool-copy-actions";
    if (features.copy && tool.input !== undefined) {
      copyActions.append(makeCopyButton("Copy tool input", toolLiteralText(tool.input), parent, "tool-copy-button"));
    }
    if (features.copy && tool.output !== undefined) {
      copyActions.append(makeCopyButton("Copy tool result", toolOutputPresentation(tool, tool.output).text, parent, "tool-copy-button"));
    }
    if (features.copy && tool.error !== undefined) {
      copyActions.append(makeCopyButton("Copy tool error", toolOutputPresentation(tool, tool.error).text, parent, "tool-copy-button"));
    }
    if (copyActions.childElementCount) parent.append(copyActions);
  }

  function appendToolInspector(article, tool, toolIndex) {
    const card = document.createElement("section");
    card.className = `tool-inspector tool-inspector-${toolDisplayStatus(tool)}`;
    card.dataset.toolIndex = String(toolIndex);
    card.dataset.toolUseId = String(tool.id || "");
    const disclosureId = `tool-details-${Math.random().toString(36).slice(2)}`;
    const expanded = state.expandedTools.has(tool.identity);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "tool-disclosure";
    button.dataset.focusKey = tool.identity;
    button.setAttribute("aria-expanded", String(expanded));
    button.setAttribute("aria-controls", disclosureId);
    setToolDisclosureLabel(button, tool, expanded);
    const details = document.createElement("div");
    details.className = "tool-detail-body";
    details.id = disclosureId;
    details.hidden = !expanded;
    card.append(button, details);
    if (expanded) renderToolDetails(details, tool);
    button.addEventListener("click", () => {
      const anchor = findScrollAnchor(ui.transcript);
      const previousTop = ui.transcript.scrollTop;
      const open = button.getAttribute("aria-expanded") !== "true";
      button.setAttribute("aria-expanded", String(open));
      setToolDisclosureLabel(button, tool, open);
      details.hidden = !open;
      if (open) {
        state.expandedTools.add(tool.identity);
        renderToolDetails(details, tool);
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

  function groupFinishedToolRows(article, item) {
    if (!features.toolInspection || item.active) return;
    const rows = [...article.querySelectorAll(":scope > .tool-inspector")];
    if (rows.length < 2) return;
    const rowById = new Map(rows.map((row) => [row.dataset.toolUseId, row]));
    const rowByIndex = new Map(rows.map((row) => [Number(row.dataset.toolIndex), row]));
    const included = new Set();
    const groups = [];
    let run = [];
    const flush = () => {
      if (run.length > 1) groups.push(run);
      run = [];
    };
    const addRow = (row) => {
      if (!row || included.has(row)) return;
      included.add(row);
      run.push(row);
    };
    for (const round of item.toolRounds || []) {
      for (const block of round.content || []) {
        if (block?.type === "text" && typeof block.text === "string" && block.text.trim()) flush();
        if (block?.type === "tool_use") addRow(rowById.get(String(block.id ?? "")));
      }
    }
    for (const [toolIndex, tool] of (item.tools || []).entries()) addRow(rowById.get(String(tool.id || "")) || rowByIndex.get(toolIndex));
    flush();

    for (const runRows of groups) {
      const toolIdentities = runRows.map((row) => item.tools?.[Number(row.dataset.toolIndex)]?.identity || row.dataset.toolUseId);
      const identity = JSON.stringify([state.sessionId, item.turnIndex ?? item.key, "tool-group", ...toolIdentities]);
      const expanded = state.expandedTools.has(identity);
      const disclosureId = `tool-group-details-${Math.random().toString(36).slice(2)}`;
      const group = document.createElement("section");
      group.className = "tool-call-group";
      group.dataset.groupKey = identity;
      const button = document.createElement("button");
      button.type = "button";
      button.className = "tool-group-disclosure";
      button.dataset.focusKey = identity;
      button.setAttribute("aria-expanded", String(expanded));
      button.setAttribute("aria-controls", disclosureId);
      button.textContent = `${runRows.length} tool calls`;
      button.setAttribute("aria-label", `${runRows.length} tool calls. ${expanded ? "Hide calls" : "Show calls"}`);
      const contents = document.createElement("div");
      contents.className = "tool-call-group-rows";
      contents.id = disclosureId;
      contents.hidden = !expanded;
      for (const row of runRows) contents.append(row);
      group.append(button, contents);
      article.append(group);
      button.addEventListener("click", () => {
        const anchor = findScrollAnchor(ui.transcript);
        const previousTop = ui.transcript.scrollTop;
        const open = button.getAttribute("aria-expanded") !== "true";
        button.setAttribute("aria-expanded", String(open));
        button.setAttribute("aria-label", `${runRows.length} tool calls. ${open ? "Hide calls" : "Show calls"}`);
        contents.hidden = !open;
        if (open) state.expandedTools.add(identity);
        else state.expandedTools.delete(identity);
        saveExpandedTools();
        measureTranscriptTurns();
        refreshTranscriptSpacers();
        restoreScrollAnchor(ui.transcript, anchor, previousTop);
      });
    }
  }

  function appendToolSummary(article, tool, toolIndex) {
    const summary = document.createElement("p");
    const output = toolOutputPresentation(tool);
    const status = toolDisplayStatus(tool, output);
    summary.className = `tool-summary tool-summary-${status}`;
    summary.dataset.toolIndex = String(toolIndex);
    summary.dataset.toolUseId = String(tool.id || "");
    summary.setAttribute("role", "status");
    let outcome = `Tool ${tool.name || "tool"}: ${toolStatusLabel(status)}.`;
    if (output.exitCode !== null && output.exitCode !== 0) outcome += ` exit ${output.exitCode}.`;
    if (["failed", "denied"].includes(status)) {
      const error = tool.error ?? tool.output;
      const errorText = error === undefined ? "" : toolOutputPresentation(tool, error).text;
      outcome += error === undefined ? " No error detail was recorded." : ` ${errorText.slice(0, 240)}`;
    } else if (["missing", "interrupted", "unavailable"].includes(status)) {
      outcome += " No tool result was recorded.";
    } else if (status === "pending") {
      outcome += " Tool result is pending.";
    } else if (tool.output !== undefined) {
      outcome += output.text.length > 180
        ? ` Result preview (${output.text.length.toLocaleString()} characters): ${output.text.slice(0, 150)}…`
        : ` Result: ${output.text}`;
    }
    const omission = toolOmissionSummary(tool.output ?? tool.error);
    if (omission) outcome += ` ${omission}`;
    summary.textContent = outcome;
    article.append(summary);
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
      showTransientToast(ui["copy-status"], "Copied to clipboard.");
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
      showTransientToast(ui["copy-status"], "Clipboard copy failed. Select the displayed text and copy it manually.");
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
    if (tool.name === "bash") {
      const input = toolInputRecord(tool);
      appendToolField(parent, "Arguments", typeof input.command === "string" ? input.command : toolLiteralText(tool.input), true);
    } else {
      appendToolField(parent, "Arguments", toolLiteralText(tool.input));
    }
    const value = tool.error ?? tool.output;
    const envelope = bashResultEnvelope(tool, value);
    if (envelope) {
      appendToolField(parent, "stdout", envelope.stdout === undefined ? "" : String(envelope.stdout));
      if (envelope.stderr !== undefined && String(envelope.stderr)) {
        appendToolField(parent, "stderr", String(envelope.stderr));
      }
    } else if (tool.error !== undefined || (["failed", "denied"].includes(tool.status) && tool.output !== undefined)) {
      appendToolField(parent, "Error", toolLiteralText(value));
    } else if (tool.output !== undefined) appendToolField(parent, "Result", toolLiteralText(tool.output));
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

  function appendToolField(parent, label, value, code = false) {
    const field = document.createElement("div");
    field.className = "tool-detail-field";
    const heading = document.createElement("strong");
    heading.textContent = label;
    const literal = document.createElement("pre");
    if (code) {
      const codeElement = document.createElement("code");
      codeElement.textContent = value;
      literal.append(codeElement);
    } else literal.textContent = value;
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
    if (features.copy) wrapper.append(makeCopyButton("Copy code", rawCodeText, wrapper, "code-copy-button"));
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

  function render(model, nextActions) {
    actions = nextActions;
    const nextSessionId = model.selectedSessionId;
    if (state.sessionId !== nextSessionId) {
      const hadSession = state.sessionId !== null;
      state.sessionId = nextSessionId;
      state.expandedTools = nextSessionId ? readExpandedTools(nextSessionId) : new Set();
      state.transcriptHeights = new Map();
      state.transcriptAverageHeight = INITIAL_TURN_HEIGHT;
      state.transcriptRange = null;
      state.restoreReadingPosition = nextSessionId ? readReadingPosition(nextSessionId) || { atBottom: true } : null;
      state.followTranscriptToBottom = false;
      state.renderedSessionSignature = "";
      state.transcriptSearchMatches = [];
      state.transcriptSearchIndex = -1;
      if (hadSession && features.copy) clearTransientToast(ui["copy-status"]);
      if (hadSession && features.download) clearTransientToast(ui["download-status"]);
      if (features.transcriptSearch) {
        ui["transcript-search"].value = "";
        ui["transcript-find-bar"].hidden = true;
        ui["open-transcript-search"].setAttribute("aria-expanded", "false");
      }
    }
    Object.assign(state, {
      sessions: model.sessions, snapshot: model.snapshot, active: model.active, pending: model.pending,
      transcriptItems: model.transcript, draft: model.draft, workspaceDraft: model.workspaceDraft,
      recoveryWorkspaceDraft: model.recoveryWorkspaceDraft, renameDraft: model.renameDraft,
      connectionText: model.connection.text, reconnecting: model.connection.reconnecting,
      submitting: model.submitting, startBusy: model.startBusy, renamingSessionId: model.renamingSessionId,
      sessionListError: model.errors.sessionList, setupError: model.errors.setup, sendError: model.errors.send,
      renameError: model.errors.rename, recoveryError: model.errors.recovery, showRecoveryForm: model.showRecoveryForm,
      loadingSession: model.loadingSession, status: model.status, availability: model.availability,
      retryableTurnIndices: model.retryableTurnIndices, renderOptions: model.renderOptions || {},
    });
    if (state.draft !== ui.prompt.value) ui.prompt.value = state.draft;
    if (state.workspaceDraft !== ui["workspace-input"].value) ui["workspace-input"].value = state.workspaceDraft;
    if (ui["recovery-workspace-input"] && state.recoveryWorkspaceDraft !== ui["recovery-workspace-input"].value) {
      ui["recovery-workspace-input"].value = state.recoveryWorkspaceDraft;
    }
    if (ui["rename-input"] && state.renameDraft !== ui["rename-input"].value) ui["rename-input"].value = state.renameDraft;
    const platform = navigator.userAgentData?.platform || navigator.platform || "";
    ui.prompt.placeholder = /mac/i.test(platform)
      ? "Enter for a new line · ⌘+Enter to send"
      : "Enter for a new line · Ctrl+Enter to send";
    adjustTextarea();
    renderAll(state.renderOptions);
  }

  function save() { saveReadingPosition(); saveExpandedTools(); }
  function rebindSessionId(oldId, actualId) {
    for (const prefix of [readingPrefix, inspectionPrefix]) {
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
      } catch { return key; }
    }));
    state.sessionId = actualId;
    saveExpandedTools();
  }
  function focus(name) {
    const targets = { workspace: ui["workspace-input"], rename: ui["rename-input"], recovery: ui["recovery-workspace-input"], transcript: ui.transcript };
    targets[name]?.focus({ preventScroll: name === "transcript" });
    if (name === "rename") targets[name]?.select();
  }

  function dispose() {
    window.removeEventListener("resize", adjustTextarea);
    if (actions.saveCurrentSessionView) window.removeEventListener("pagehide", actions.saveCurrentSessionView);
    if (actions.online) window.removeEventListener("online", actions.online);
    if (elapsedTimer !== null) {
      window.clearInterval(elapsedTimer);
      elapsedTimer = null;
    }
  }

  function bind(nextActions) {
    actions = nextActions;
    ui["start-form"].addEventListener("submit", (event) => { event.preventDefault(); actions.startConversation(ui["workspace-input"].value); });
    ui["new-conversation"].addEventListener("click", actions.openNewConversation);
    ui["session-list"].addEventListener("click", (event) => {
      const target = event.target.closest("[data-session-id][data-action]");
      if (!target) return;
      if (target.dataset.action === "open") actions.selectSession(target.dataset.sessionId);
      else if (target.dataset.action === "rename") actions.beginRename(target.dataset.sessionId);
    });
    if (features.titleFilter) ui["session-filter"].addEventListener("input", () => { state.sessionListFilter = ui["session-filter"].value; renderSessionList(); });
    ui["workspace-input"].addEventListener("input", () => actions.updateWorkspaceDraft(ui["workspace-input"].value));
    ui["rename-input"].addEventListener("input", () => actions.updateRenameDraft(ui["rename-input"].value));
    ui["recovery-workspace-input"].addEventListener("input", () => actions.updateRecoveryWorkspaceDraft(ui["recovery-workspace-input"].value));
    ui["rename-form"].addEventListener("submit", (event) => { event.preventDefault(); actions.renameSession(ui["rename-input"].value); });
    ui["cancel-rename"].addEventListener("click", actions.cancelRename);
    ui["set-workspace-form"].addEventListener("submit", (event) => { event.preventDefault(); actions.setSessionWorkspace(ui["recovery-workspace-input"].value); });
    ui["cancel-workspace"].addEventListener("click", actions.cancelWorkspace);
    ui.conversation.addEventListener("click", (event) => {
      const target = event.target.closest("[data-action]");
      if (target?.dataset.action === "show-workspace") actions.showWorkspaceForm();
      else if (target?.dataset.action === "retry-recorded-turn") actions.retryRecordedTurn(Number(target.dataset.turnIndex));
    });
    ui.composer.addEventListener("submit", (event) => { event.preventDefault(); actions.submitPrompt(); });
    ui.prompt.addEventListener("input", () => actions.updateDraft(ui.prompt.value));
    let composition = false;
    ui.prompt.addEventListener("compositionstart", () => { composition = true; });
    ui.prompt.addEventListener("compositionend", () => { composition = false; });
    ui.prompt.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || (!event.ctrlKey && !event.metaKey) || composition || event.isComposing || event.keyCode === 229) return;
      event.preventDefault(); actions.submitPrompt();
    });
    window.addEventListener("resize", adjustTextarea);
    ui["retry-submission"].addEventListener("click", actions.retrySubmission);
    ui["stop-turn"].addEventListener("click", actions.stopTurn);
    ui["new-content"].addEventListener("click", showNewContent);
    ui.transcript.addEventListener("pointerdown", () => { state.followTranscriptToBottom = false; });
    ui.transcript.addEventListener("wheel", () => { state.followTranscriptToBottom = false; }, { passive: true });
    ui.transcript.addEventListener("touchstart", () => { state.followTranscriptToBottom = false; }, { passive: true });
    ui.transcript.addEventListener("keydown", transcriptKeydown);
    ui.transcript.addEventListener("scroll", () => { saveReadingPosition(); scheduleTranscriptRender(); });
    if (features.transcriptSearch) {
      ui["open-transcript-search"].addEventListener("click", openTranscriptSearch);
      ui["close-transcript-search"].addEventListener("click", closeTranscriptSearch);
      ui.conversation.addEventListener("keydown", conversationKeydown);
      ui["transcript-find-bar"].addEventListener("keydown", searchKeydown);
      ui["transcript-search"].addEventListener("input", () => renderTranscriptSearch({ reset: true, navigate: true }));
      ui["transcript-search-prev"].addEventListener("click", () => moveTranscriptSearch(-1));
      ui["transcript-search-next"].addEventListener("click", () => moveTranscriptSearch(1));
      ui["transcript-search-clear"].addEventListener("click", clearSearch);
    }
    if (features.download) ui["download-transcript"].addEventListener("click", downloadTranscript);
    window.addEventListener("pagehide", actions.saveCurrentSessionView);
    window.addEventListener("online", actions.online);
  }

  return { bind, render, save, focus, rebindSessionId, dispose };
}
