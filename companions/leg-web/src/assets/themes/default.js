import { createSharedRenderer } from "/themes/shared-renderer.js";

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
  const features = Object.freeze({
    titleFilter: true,
    transcriptSearch: true,
    copy: true,
    download: true,
    toolInspection: true,
  });
  const renderer = createSharedRenderer(elements, features);
  return {
    features,
    bind(actions) { renderer.bind(actions); },
    render(model, actions) {
      root.dataset.themeId = "default";
      root.dataset.turnState = model.status.text.toLowerCase();
      renderer.render(model, actions);
    },
    save: renderer.save,
    focus: renderer.focus,
    rebindSessionId: renderer.rebindSessionId,
  };
}

export const theme = Object.freeze({
  id: "default",
  mount,
});
