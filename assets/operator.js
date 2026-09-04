"use strict";

const POLL_MS = 2000;

const STAGE_PILL = {
  invited: ["pill", "invited"],
  joined: ["pill wait", "joined"],
  routed: ["pill wait", "routed"],
  flowing: ["pill live", "live"],
  "open-live": ["pill good", "in open-live"],
  left: ["pill", "left"],
  expired: ["pill", "expired"],
  revoked: ["pill bad", "revoked"],
};

const STEP_STATES = ["pending", "active", "done", "failed"];

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "always", style: "short" });

const seatList = document.getElementById("seats");
const seatsEmpty = document.getElementById("seats-empty");
const metaLine = document.getElementById("meta");
const feedLine = document.getElementById("feed");
const form = document.getElementById("invite");
const nameInput = document.getElementById("name");
const ttlSelect = document.getElementById("ttl");
const createButton = document.getElementById("create");
const formError = document.getElementById("form-error");
const handover = document.getElementById("handover");
const handoverName = document.getElementById("handover-name");
const handoverUrl = document.getElementById("handover-url");
const handoverCopy = document.getElementById("handover-copy");

const rows = new Map();
const flashTimers = new WeakMap();
let polling = false;
let openLive = null;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function setText(node, text) {
  const value = text === null || text === undefined ? "" : String(text);
  if (node.textContent !== value) node.textContent = value;
}

function setClass(node, className) {
  if (node.className !== className) node.className = className;
}

function flash(button, label) {
  if (!flashTimers.has(button)) button.dataset.label = button.textContent;
  clearTimeout(flashTimers.get(button));
  button.textContent = label;
  flashTimers.set(button, setTimeout(() => {
    button.textContent = button.dataset.label;
    flashTimers.delete(button);
  }, 1400));
}

function copyLink(input, button) {
  const clipboard = navigator.clipboard;
  const written = clipboard && clipboard.writeText
    ? clipboard.writeText(input.value)
    : Promise.reject(new Error("clipboard unavailable"));
  written.then(
    () => flash(button, "Copied"),
    () => {
      input.focus();
      input.select();
      flash(button, "Selected");
    },
  );
}

function relative(iso) {
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return "";
  const secs = (at - Date.now()) / 1000;
  const abs = Math.abs(secs);
  const scale = abs < 60 ? ["second", 1]
    : abs < 3600 ? ["minute", 60]
      : abs < 86400 ? ["hour", 3600]
        : ["day", 86400];
  return rtf.format(Math.round(secs / scale[1]), scale[0]);
}

function expiryText(iso) {
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return "";
  return (at > Date.now() ? "expires " : "expired ") + relative(iso);
}

function stagePill(stage) {
  return STAGE_PILL[stage] || ["pill", stage || "unknown"];
}

function stepDetail(step) {
  if (step.detail) return step.detail;
  if (step.step === "open-live" && !openLive) return "open-live not watched";
  return "";
}

function railKey(steps) {
  return steps.map((step) => step.step).join("|");
}

function renderChain(row, steps) {
  const key = railKey(steps);
  if (key !== row.railKey) {
    row.chain.textContent = "";
    row.steps = steps.map((step) => {
      const node = el("div", "step");
      const body = el("span", "body");
      const label = el("span", "label", step.step);
      const detail = el("span", "detail");
      body.append(label, detail);
      node.append(el("span", "dot"), body);
      row.chain.append(node);
      return { node, detail };
    });
    row.railKey = key;
  }
  steps.forEach((step, index) => {
    const entry = row.steps[index];
    const state = STEP_STATES.includes(step.state) ? step.state : "pending";
    setClass(entry.node, "step " + state);
    setText(entry.detail, stepDetail(step));
  });
}

function revoke(seat, label, button) {
  if (!window.confirm("Revoke the invite for " + (label || seat) + "?")) return;
  button.disabled = true;
  fetch("/api/seats/" + encodeURIComponent(seat), { method: "DELETE" }).then(
    (res) => {
      if (!res.ok && res.status !== 404) flash(button, "Failed");
      button.disabled = false;
      refresh();
    },
    () => {
      button.disabled = false;
      feedLine.hidden = false;
    },
  );
}

function createRow(seat) {
  const root = el("article", "seat");

  const head = el("div", "seat-head");
  const name = el("span", "name");
  const sid = el("span", "mono faint sid");
  const pill = el("span", "pill");
  head.append(name, sid, el("span", "spacer"), pill);

  const chain = el("div", "chain");

  const linkRow = el("div", "link-row");
  const url = el("input", "mono url");
  url.readOnly = true;
  url.spellcheck = false;
  url.setAttribute("aria-label", "Join link");
  const copy = el("button", "copy", "Copy link");
  copy.type = "button";
  copy.addEventListener("click", () => copyLink(url, copy));
  const expiry = el("span", "expiry faint");
  const revokeButton = el("button", "revoke", "Revoke");
  revokeButton.type = "button";
  revokeButton.addEventListener("click", () => revoke(seat.seat, name.textContent, revokeButton));
  linkRow.append(url, copy, expiry, el("span", "spacer"), revokeButton);

  root.append(head, chain, linkRow);
  return { root, name, sid, pill, chain, url, expiry, steps: [], railKey: "" };
}

function updateRow(row, seat) {
  setText(row.name, seat.display_name || seat.seat);
  setText(row.sid, seat.seat);
  const pill = stagePill(seat.stage);
  setClass(row.pill, pill[0]);
  setText(row.pill, pill[1]);
  const url = seat.join_url || "";
  if (row.url.value !== url) row.url.value = url;
  setText(row.expiry, expiryText(seat.expires_at));
  renderChain(row, Array.isArray(seat.chain) ? seat.chain : []);
}

// insertBefore only where the order differs, so a steady list never has a node
// detached and reattached under it — that would blur a focused copy button.
function syncOrder(container, ordered) {
  let cursor = container.firstChild;
  for (const node of ordered) {
    if (node === cursor) {
      cursor = cursor.nextSibling;
      continue;
    }
    container.insertBefore(node, cursor);
  }
}

function renderSeats(payload) {
  const seats = (Array.isArray(payload.seats) ? payload.seats : [])
    .filter((seat) => seat && typeof seat.seat === "string")
    .sort((a, b) => (Date.parse(b.created_at) || 0) - (Date.parse(a.created_at) || 0));

  const ordered = [];
  const seen = new Set();
  for (const seat of seats) {
    if (seen.has(seat.seat)) continue;
    seen.add(seat.seat);
    let row = rows.get(seat.seat);
    if (!row) {
      row = createRow(seat);
      rows.set(seat.seat, row);
    }
    updateRow(row, seat);
    ordered.push(row.root);
  }

  for (const [id, row] of rows) {
    if (seen.has(id)) continue;
    row.root.remove();
    rows.delete(id);
  }

  syncOrder(seatList, ordered);
  seatsEmpty.hidden = ordered.length > 0;
}

function renderMeta(payload) {
  const target = payload.target || {};
  const parts = [];
  const where = [target.node, target.network].filter(Boolean).join(" / ");
  parts.push(where || "no target");
  parts.push(openLive ? "open-live " + openLive : "open-live not watched");
  if (payload.reconciled_at) parts.push("reconciled " + relative(payload.reconciled_at));
  setText(metaLine, parts.join(" · "));
}

function refresh() {
  if (polling) return Promise.resolve();
  polling = true;
  return fetch("/api/seats", { headers: { accept: "application/json" } })
    .then((res) => {
      if (!res.ok) throw new Error("HTTP " + res.status);
      return res.json();
    })
    .then((payload) => {
      openLive = typeof payload.open_live === "string" && payload.open_live ? payload.open_live : null;
      renderMeta(payload);
      renderSeats(payload);
      feedLine.hidden = true;
    })
    .catch(() => {
      feedLine.hidden = false;
    })
    .then(() => {
      polling = false;
    });
}

function showFormError(message) {
  setText(formError, message || "");
  formError.hidden = !message;
}

function showHandover(seat) {
  setText(handoverName, seat.display_name || seat.seat);
  handoverUrl.value = seat.join_url || "";
  handover.hidden = false;
  handoverUrl.focus();
  handoverUrl.select();
}

function createInvite() {
  const name = nameInput.value.trim();
  if (!name) {
    showFormError("Enter a guest name.");
    nameInput.focus();
    return;
  }
  const body = JSON.stringify({ name, ttl_secs: Number(ttlSelect.value) });
  createButton.disabled = true;
  fetch("/api/invites", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body,
  })
    .then((res) => res.json().then(
      (parsed) => ({ res, parsed }),
      () => ({ res, parsed: null }),
    ))
    .then(({ res, parsed }) => {
      if (!res.ok) {
        showFormError((parsed && parsed.error) || "Could not create the invite (HTTP " + res.status + ").");
        return;
      }
      showFormError("");
      nameInput.value = "";
      if (parsed) showHandover(parsed);
      refresh();
    })
    .catch(() => {
      showFormError("Could not reach the gateway.");
    })
    .then(() => {
      createButton.disabled = false;
    });
}

form.addEventListener("submit", (event) => {
  event.preventDefault();
  createInvite();
});

handoverCopy.addEventListener("click", () => copyLink(handoverUrl, handoverCopy));

refresh();
setInterval(refresh, POLL_MS);
