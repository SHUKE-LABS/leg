const elementIds = [
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
  "open-transcript-search",
  "close-transcript-search",
  "workspace-warning",
  "connection-message",
  "transcript-warnings",
  "transcript",
  "transcript-find-bar",
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
];

async function mount(root) {
  const response = await fetch("/themes/default.html", { cache: "no-store" });
  if (!response.ok) throw new Error("default_view_unavailable");
  root.innerHTML = await response.text();
  const elements = Object.fromEntries(
    elementIds.map((id) => [id, root.querySelector(`#${id}`)]),
  );
  return createView(root, elements);
}

function createView(root, elements) {
  return {
    elements,
    features: Object.freeze({
      titleFilter: true,
      transcriptSearch: true,
      copy: true,
      download: true,
      toolInspection: true,
    }),
    bind(actions) {
      const ui = elements;
      ui["start-form"].addEventListener("submit", actions.startConversation);
      ui["new-conversation"].addEventListener("click", actions.openNewConversation);
      ui["session-list"].addEventListener("click", actions.sessionListClick);
      ui["session-filter"].addEventListener("input", actions.filterSessions);
      ui["rename-form"].addEventListener("submit", actions.renameSession);
      ui["cancel-rename"].addEventListener("click", actions.cancelRename);
      ui["set-workspace-form"].addEventListener("submit", actions.setSessionWorkspace);
      ui["cancel-workspace"].addEventListener("click", actions.cancelWorkspace);
      ui.conversation.addEventListener("click", actions.conversationClick);
      ui.composer.addEventListener("submit", actions.submitForm);
      ui.prompt.addEventListener("input", actions.updateDraft);
      ui.prompt.addEventListener("compositionstart", actions.compositionStart);
      ui.prompt.addEventListener("compositionend", actions.compositionEnd);
      ui.prompt.addEventListener("keydown", actions.promptKeydown);
      window.addEventListener("resize", actions.resize);
      ui["retry-submission"].addEventListener("click", actions.retrySubmission);
      ui["open-transcript-search"].addEventListener("click", actions.openTranscriptSearch);
      ui["close-transcript-search"].addEventListener("click", actions.closeTranscriptSearch);
      ui.conversation.addEventListener("keydown", actions.conversationKeydown);
      ui["transcript-find-bar"].addEventListener("keydown", actions.searchKeydown);
      ui["transcript-search"].addEventListener("input", actions.searchInput);
      ui["transcript-search-prev"].addEventListener("click", actions.searchPrevious);
      ui["transcript-search-next"].addEventListener("click", actions.searchNext);
      ui["transcript-search-clear"].addEventListener("click", actions.clearSearch);
      ui["download-transcript"].addEventListener("click", actions.downloadTranscript);
      ui["stop-turn"].addEventListener("click", actions.stopTurn);
      ui["new-content"].addEventListener("click", actions.showNewContent);
      ui.transcript.addEventListener("pointerdown", actions.transcriptPointerDown);
      ui.transcript.addEventListener("wheel", actions.transcriptPointerDown, { passive: true });
      ui.transcript.addEventListener("touchstart", actions.transcriptPointerDown, { passive: true });
      ui.transcript.addEventListener("keydown", actions.transcriptKeydown);
      ui.transcript.addEventListener("scroll", actions.transcriptScroll);
      window.addEventListener("pagehide", actions.saveCurrentSessionView);
      window.addEventListener("online", actions.online);
    },
    render(model) {
      root.dataset.themeId = "default";
      root.dataset.turnState = model.status.text.toLowerCase();
    },
  };
}

export const theme = Object.freeze({
  id: "default",
  mount,
});
