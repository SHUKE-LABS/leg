import { themes as registeredThemes } from "/themes/registry.js";
import { startController } from "/controller.js";

const themeStorageKey = "leg-web-theme";
let saveThemeViewState = () => {};

function setStartupError(message) {
  const error = document.getElementById("theme-startup-error");
  error.textContent = message;
  error.hidden = false;
}

function buildThemeSelector(records, selectedId) {
  const root = document.getElementById("theme-selector-root");
  root.replaceChildren();
  if (records.length < 2) return null;
  const label = document.createElement("label");
  label.className = "theme-selector";
  label.append("Interface theme ");
  const select = document.createElement("select");
  select.setAttribute("aria-label", "Interface theme");
  for (const record of records) {
    const option = document.createElement("option");
    option.value = record.id;
    option.textContent = record.name;
    select.append(option);
  }
  select.value = selectedId;
  select.disabled = true;
  select.addEventListener("change", () => {
    if (!records.some((record) => record.id === select.value)) return;
    saveThemeViewState();
    window.sessionStorage.setItem(themeStorageKey, select.value);
    window.location.reload();
  });
  label.append(select);
  root.append(label);
  return select;
}

async function loadTheme(record) {
  const root = document.getElementById("theme-root");
  root.replaceChildren();
  root.dataset.themeId = record.id;
  document.getElementById("theme-startup-error").hidden = true;
  document.getElementById("theme-style")?.remove();

  const link = document.createElement("link");
  link.id = "theme-style";
  link.rel = "stylesheet";
  link.href = record.stylesheet;
  const stylesheetReady = new Promise((resolve, reject) => {
    const timeout = window.setTimeout(() => reject(new Error("theme_stylesheet_timeout")), 5000);
    link.addEventListener("load", () => { window.clearTimeout(timeout); resolve(); }, { once: true });
    link.addEventListener("error", () => { window.clearTimeout(timeout); reject(new Error("theme_stylesheet_unavailable")); }, { once: true });
  });
  document.head.append(link);
  await stylesheetReady;

  const module = await record.load();
  if (module.theme?.id !== record.id || typeof module.theme?.mount !== "function") {
    throw new Error("theme_contract_invalid");
  }
  const view = await module.theme.mount(root);
  if (!view || typeof view.bind !== "function" || typeof view.render !== "function" ||
      typeof view.save !== "function" || typeof view.focus !== "function" ||
      typeof view.rebindSessionId !== "function") {
    throw new Error("theme_contract_invalid");
  }
  view.record = record;
  view.features = Object.freeze({ ...(view.features || {}) });
  return view;
}

async function initializeTheme() {
  const unique = new Map();
  for (const record of registeredThemes) {
    if (record && typeof record.id === "string" && record.id && !unique.has(record.id)) {
      unique.set(record.id, record);
    }
  }
  const records = [...unique.values()];
  const defaultRecord = unique.get("default");
  if (!defaultRecord) {
    setStartupError("The default interface is not registered. The Web page cannot start.");
    return null;
  }

  const storedId = window.sessionStorage.getItem(themeStorageKey);
  let selected = unique.get(storedId) || defaultRecord;
  if (storedId && !unique.has(storedId)) {
    window.sessionStorage.setItem(themeStorageKey, defaultRecord.id);
  }
  const selector = buildThemeSelector(records, selected.id);
  try {
    const view = await loadTheme(selected);
    if (selector) selector.disabled = false;
    return view;
  } catch (error) {
    document.getElementById("theme-style")?.remove();
    document.getElementById("theme-root").replaceChildren();
    if (selected.id !== defaultRecord.id) {
      const failedName = selected.name;
      window.sessionStorage.setItem(themeStorageKey, defaultRecord.id);
      if (selector) selector.value = defaultRecord.id;
      try {
        const view = await loadTheme(defaultRecord);
        const notice = document.getElementById("theme-startup-error");
        notice.textContent = `${failedName} could not start. The Default theme was opened instead.`;
        notice.hidden = false;
        if (selector) selector.disabled = false;
        return view;
      } catch {
        document.getElementById("theme-style")?.remove();
        setStartupError("The selected theme and the Default theme could not start. Check the embedded Web assets, then refresh.");
        return null;
      }
    }
    setStartupError("The Default theme could not start. Check the embedded Web assets, then refresh.");
    return null;
  }
}

const initialView = await initializeTheme();
if (initialView) saveThemeViewState = startController(initialView);
