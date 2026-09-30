const tokenKey = "leg-web-launch-token";
const bootstrap = window.location.hash.slice(1);
if (/^[0-9a-f]{64}$/i.test(bootstrap)) {
  window.sessionStorage.setItem(tokenKey, bootstrap);
  window.history.replaceState(null, "", window.location.pathname + window.location.search);
}

const token = window.sessionStorage.getItem(tokenKey);
const status = document.querySelector("#status");
const output = document.querySelector("#sessions");

async function loadSessions() {
  if (!token) {
    status.textContent = "Open the launch URL again to authorize this tab.";
    return;
  }
  const response = await fetch("/api/sessions", {
    headers: { Authorization: `Bearer ${token}` },
    cache: "no-store",
  });
  if (!response.ok) {
    status.textContent = "The local host rejected this request.";
    return;
  }
  output.textContent = JSON.stringify(await response.json(), null, 2);
  status.textContent = "Connected to the local Leg host.";
}

loadSessions().catch(() => { status.textContent = "The local host is unavailable."; });
