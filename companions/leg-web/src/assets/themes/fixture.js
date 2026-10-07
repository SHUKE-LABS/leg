const elementIds = [
  "connection-state",
  "new-conversation",
  "session-list",
  "session-list-empty",
  "session-list-no-results",
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
  "transcript-warnings",
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
];

async function mount(root) {
  const response = await fetch("/themes/fixture.html", { cache: "no-store" });
  if (!response.ok) throw new Error("fixture_view_unavailable");
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
      titleFilter: false,
      transcriptSearch: false,
      copy: false,
      download: false,
      toolInspection: false,
    }),
    bind(actions) {
      const ui = elements;
      ui["start-form"].addEventListener("submit", actions.startConversation);
      ui["new-conversation"].addEventListener("click", actions.openNewConversation);
      ui["session-list"].addEventListener("click", actions.sessionListClick);
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
      root.dataset.themeId = "fixture";
      root.dataset.turnState = model.status.text.toLowerCase();
      const state = elements["turn-status"];
      state.dataset.busy = String(model.status.busy);
    },
  };
}

export const theme = Object.freeze({
  id: "fixture",
  mount,
});
