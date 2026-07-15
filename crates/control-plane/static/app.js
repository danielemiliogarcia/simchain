/* Schema-driven control plane: the form comes from /api/v1/config/schema and
 * is populated from /api/v1/config; nothing here hard-codes individual settings
 * beyond the UI-only ignore rules below. */
"use strict";

const TOKEN = window.CONTROL_PLANE_TOKEN;
const $ = (sel) => document.querySelector(sel);

let schema = null;          // [{key, default, group, scope, control, options, optional, help, warning}]
let lastState = null;       // last /api/v1/config payload
let dirty = new Map();      // key -> edited value (string)
let fieldErrors = new Map(); // key -> latest client/server validation message
let applying = false;
let latestStatus = null;
let changingMiningState = false;

const GROUP_TITLES = {
  "mining": "Mining",
  "spam-basics": "Spam basics",
  "spam-advanced": "Spam advanced",
};

/* UI-only relevance rules: fields that are ignored given other staged values. */
function ignoredReason(key, values) {
  const dataMode = Number(values.get("SPAM_TX_DATA_MAX_BYTES") || "0") > 0;
  const fanoutAuto = values.get("SPAM_FANOUT_AUTO") === "true";
  if (key === "SPAM_FANOUT_UTXOS" && fanoutAuto) return "ignored: SPAM_FANOUT_AUTO=true";
  if ((key === "SPAM_FIXED_TXS_PER_BLOCK" || key === "SPAM_SENDMANY_OUTPUTS") && dataMode)
    return "ignored in DATA/HYBRID mode (SPAM_TX_DATA_MAX_BYTES > 0)";
  if ((key === "SPAM_TX_DATA_MIN_BYTES" || key === "SPAM_SMALL_TXS_PER_BLOCK" ||
       key === "SPAM_FLOOR_POOL_TXS" || key === "SPAM_FILL_BLOCK_RATIO") && !dataMode)
    return "ignored in OUTPUT mode (SPAM_TX_DATA_MAX_BYTES = 0)";
  return null;
}

async function api(path, options) {
  const response = await fetch(path, options);
  const body = await response.json().catch(() => null);
  return { ok: response.ok, status: response.status, body };
}

/* ------------------------------------------------------------------ status */

function fmtBytes(n) {
  if (n == null) return "–";
  if (n > 1e6) return (n / 1e6).toFixed(2) + " MB";
  if (n > 1e3) return (n / 1e3).toFixed(1) + " kB";
  return n + " B";
}

function tile(k, v) {
  return `<div class="tile"><div class="k">${k}</div><div class="v">${v}</div></div>`;
}

function renderStatus(s) {
  latestStatus = s;
  const stale = !s.last_updated_ms || (Date.now() - s.last_updated_ms) > 8000;
  const conn = $("#conn");
  conn.textContent = stale ? (s.last_error ? `stale: ${s.last_error}` : "stale / RPC unavailable")
                           : `live · height ${s.height ?? "?"}${s.last_error ? ` · warning: ${s.last_error}` : ""}`;
  conn.className = "conn " + (stale || s.last_error ? "stale" : "ok");

  const cadence = s.cadence ? `${s.cadence.mean_secs.toFixed(1)}s (n=${s.cadence.samples})` : "–";
  const mp = s.mempool;
  $("#tiles").innerHTML =
    tile("height", s.height ?? "–") +
    tile("cadence", cadence) +
    tile("mempool txs", mp ? mp.tx_count : "–") +
    tile("mempool size", mp ? fmtBytes(mp.vbytes) + " vB" : "–") +
    tile("min fee", mp ? (mp.min_fee * 1e5).toFixed(1) + " sat/vB" : "–") +
    tile("best hash", s.best_hash ? s.best_hash.slice(0, 12) + "…" : "–");

  const rows = (s.recent_blocks || []).map((b) =>
    `<tr><td>${b.height}</td><td>${b.delta_secs == null ? "–" : Math.max(0, b.delta_secs) + "s"}</td>` +
    `<td>${b.tx_count}</td><td>${fmtBytes(b.size_bytes)}</td><td>${b.weight}</td></tr>`).join("");
  $("#blocks tbody").innerHTML = rows || `<tr><td colspan="5">no blocks yet</td></tr>`;

  const max = Math.max(1, ...(s.fee_histogram || []).map((b) => b.count));
  $("#fees").innerHTML = (s.fee_histogram || []).map((b) =>
    `<div class="bar-row"><span class="lbl">${b.label}</span>` +
    `<div class="bar" style="width:${(100 * b.count / max).toFixed(1)}%"></div>` +
    `<span class="n">${b.count}</span></div>`).join("") || "–";

  $("#services").innerHTML = Object.entries(s.components || {}).map(([name, svc]) => {
    let cls = "off", text = svc.phase || svc.status;
    if (svc.restarting) { cls = "err"; text = "restarting"; }
    else if (svc.running) { cls = "ok"; }
    else if (svc.status === "exited") { cls = svc.exit_code === 0 ? "warn" : "err"; text = `exited(${svc.exit_code})`; }
    else if (!svc.present) { text = "absent"; }
    const details = [];
    if (svc.effective_generation != null) details.push(`gen ${svc.effective_generation}`);
    if (svc.uptime_secs != null) details.push(`up ${svc.uptime_secs}s`);
    return `<div class="svc"><span class="dot ${cls}"></span>${name.replace("btc-simnet-", "")} · ${text}` +
      `${details.length ? " · " + details.join(" · ") : ""}</div>`;
  }).join("");

  const mining = (s.components || {})["btc-simnet-mining-controller"];
  const miningState = $("#mining-state");
  const pause = $("#mining-pause");
  const resume = $("#mining-resume");
  if (!mining || !mining.present) {
    miningState.textContent = mining && mining.last_error
      ? `mining worker unreachable: ${mining.last_error}` : "mining worker unavailable";
    pause.disabled = true;
    resume.disabled = true;
  } else {
    const desired = mining.desired_state || "unknown";
    const effective = mining.effective_state || "unknown";
    const next = mining.next_scheduled_attempt_ms == null
      ? "" : ` · next attempt ${new Date(mining.next_scheduled_attempt_ms).toLocaleTimeString()}`;
    miningState.textContent = `desired ${desired} · effective ${effective} · phase ${mining.phase || mining.status}${next}`;
    pause.disabled = changingMiningState || desired === "paused";
    resume.disabled = changingMiningState || desired === "running";
  }
}

/* ---------------------------------------------------------------- settings */

function stagedValues() {
  const values = new Map(Object.entries(lastState ? lastState.desired : {}));
  for (const [k, v] of dirty) values.set(k, v);
  return values;
}

function runningValueFor(spec) {
  if (!lastState) return null;
  const svc = lastState.effective[spec.component];
  if (!svc || !svc.reachable || !svc.values) return null;
  return svc.values[spec.key] ?? "";
}

function buildForm() {
  const container = $("#form");
  container.innerHTML = "";
  const groups = new Map();
  for (const spec of schema.settings) {
    if (!groups.has(spec.group)) groups.set(spec.group, []);
    groups.get(spec.group).push(spec);
  }
  for (const [group, specs] of groups) {
    const div = document.createElement("div");
    div.className = "group";
    div.innerHTML = `<div class="gtitle">${GROUP_TITLES[group] || group}</div>`;
    for (const spec of specs) {
      const field = document.createElement("div");
      field.className = "field";
      field.dataset.key = spec.key;

      const label = document.createElement("label");
      label.textContent = spec.key;
      label.title = spec.help;

      let input;
      if (spec.control === "toggle") {
        input = document.createElement("select");
        input.innerHTML = `<option value="true">true</option><option value="false">false</option>`;
      } else if (spec.control === "choice") {
        input = document.createElement("select");
        input.innerHTML = (spec.options || []).map((o) => `<option value="${o}">${o}</option>`).join("");
      } else {
        input = document.createElement("input");
        input.type = (spec.control === "integer" || spec.control === "decimal") ? "number" : "text";
        if (spec.control === "integer") input.step = "1";
        if (spec.control === "decimal") input.step = "any";
        if (spec.minimum != null) input.min = String(spec.minimum);
        if (spec.maximum != null) input.max = String(spec.maximum);
        input.placeholder = spec.optional ? "(empty = unset)" : `default: ${spec.default}`;
      }
      input.addEventListener("input", () => onEdit(spec.key, input.value));
      input.addEventListener("change", () => onEdit(spec.key, input.value));

      const running = document.createElement("div");
      running.className = "running";
      const validation = document.createElement("div");
      validation.className = "field-error";

      field.append(label, input, running, validation);
      if (spec.warning) {
        const warn = document.createElement("div");
        warn.className = "fieldwarn";
        warn.textContent = "⚠ " + spec.warning;
        field.append(warn);
      }
      div.append(field);
    }
    container.append(div);
  }
}

function onEdit(key, value) {
  const staged = lastState ? (lastState.desired[key] ?? "") : "";
  if (value === staged) dirty.delete(key); else dirty.set(key, value);
  fieldErrors.delete(key);
  refreshForm();
}

function refreshForm() {
  if (!schema || !lastState) return;
  const values = stagedValues();
  for (const spec of schema.settings) {
    const field = document.querySelector(`.field[data-key="${spec.key}"]`);
    if (!field) continue;
    const input = field.querySelector("input, select");
    const isDirty = dirty.has(spec.key);
    if (document.activeElement !== input) {
      input.value = isDirty ? dirty.get(spec.key) : (lastState.desired[spec.key] ?? "");
    }
    field.classList.toggle("dirty", isDirty);

    const reason = ignoredReason(spec.key, values);
    field.classList.toggle("ignored", reason != null);
    field.title = reason || "";
    input.disabled = spec.key === "SPAM_FANOUT_UTXOS" && values.get("SPAM_FANOUT_AUTO") === "true";

    const validationEl = field.querySelector(".field-error");
    let validation = fieldErrors.get(spec.key) || "";
    if (!validation && !input.checkValidity()) validation = input.validationMessage;
    validationEl.textContent = validation;
    field.classList.toggle("invalid", validation !== "");

    const runningEl = field.querySelector(".running");
    const running = runningValueFor(spec);
    if (running == null) {
      runningEl.textContent = "effective: –";
      runningEl.className = "running";
    } else {
      runningEl.textContent = "effective: " + (running === "" ? "(unset)" : running);
      const differs = (lastState.desired[spec.key] ?? "") !== running;
      runningEl.className = "running" + (differs ? " differs" : "");
    }
  }

  // Impact preview: server-computed drift plus locally edited components.
  const impacted = new Set(lastState.pending_apply || []);
  for (const key of dirty.keys()) {
    const spec = schema.settings.find((s) => s.key === key);
    if (spec) impacted.add(spec.component);
  }
  const impacts = [...impacted].map((component) => {
    const edited = schema.settings.find((spec) => spec.component === component && dirty.has(spec.key));
    const mode = edited ? edited.apply_mode.replaceAll("_", " ") : "pending reconciliation";
    return `${component.replace("btc-simnet-", "")} (${mode})`;
  });
  $("#impact").textContent = impacted.size
    ? "pending apply: " + impacts.join(", ")
    : "desired and effective configuration match";
  const invalid = [...document.querySelectorAll("#form input, #form select")]
    .some((input) => !input.checkValidity()) || fieldErrors.size > 0;
  $("#apply").disabled = applying || invalid || (dirty.size === 0 && impacted.size === 0);

  const errors = [];
  if (!lastState.desired_valid) {
    const details = (lastState.desired_errors || [])
      .map((d) => `${d.key ?? ""}${d.value != null ? "=" + d.value : ""}: ${d.cause}`).join("\n");
    errors.push({ className: "pageerr", text: `desired configuration is invalid:\n${details}` });
  }
  for (const warning of lastState.warnings || []) {
    errors.push({ className: "pagewarn", text: warning });
  }
  const errorContainer = $("#page-errors");
  errorContainer.replaceChildren(...errors.map(({ className, text }) => {
    const message = document.createElement("div");
    message.className = className;
    message.textContent = text;
    return message;
  }));
}

async function refreshState() {
  const { ok, body } = await api("/api/v1/config");
  if (!ok || !body) return;
  lastState = body;
  refreshForm();
}

async function refreshStatus() {
  const { ok, body } = await api("/api/v1/status");
  if (ok && body) renderStatus(body);
}

async function setMiningState(state) {
  if (changingMiningState) return;
  changingMiningState = true;
  renderStatus(latestStatus || { components: {} });
  const result = $("#mining-action-result");
  result.textContent = `${state === "paused" ? "Pausing" : "Resuming"}…`;
  result.className = "action-result";
  try {
    const { ok, body } = await api("/api/v1/mining/state", {
      method: "PUT",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
      },
      body: JSON.stringify({ state }),
    });
    result.textContent = ok
      ? `acknowledged at phase ${body.phase}`
      : ((body && body.error && body.error.message) || "mining state change failed");
    result.className = "action-result" + (ok ? "" : " err");
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    changingMiningState = false;
    await refreshStatus();
    await refreshState();
  }
}

/* ------------------------------------------------------------------- apply */

function showResult(text, isError) {
  const el = $("#result");
  el.hidden = false;
  el.textContent = text;
  el.className = "result" + (isError ? " err" : "");
}

async function doApply() {
  if (applying) return;
  applying = true;
  const button = $("#apply");
  button.disabled = true;
  button.textContent = "Applying…";
  try {
    const settings = Object.fromEntries(dirty);
    const { ok, status, body } = await api("/api/v1/config", {
      method: "PATCH",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
      },
      body: JSON.stringify({ settings, base_generation: lastState ? lastState.generation : null }),
    });
    if (ok) {
      dirty.clear();
      fieldErrors.clear();
      showResult(JSON.stringify(body, null, 2), false);
    } else if (status === 409 && body && body.error && body.error.code === "stale_revision") {
      showResult("Conflict: desired configuration changed since this page loaded.\n" +
        "The form has been refreshed — review and apply again.\n\n" + JSON.stringify(body, null, 2), true);
    } else {
      fieldErrors.clear();
      for (const detail of (body && body.error && body.error.details) || []) {
        if (detail.key) fieldErrors.set(detail.key, detail.cause || "invalid value");
      }
      showResult(JSON.stringify(body, null, 2), true);
    }
  } catch (error) {
    showResult(String(error), true);
  } finally {
    applying = false;
    button.textContent = "Apply";
    await refreshState();
  }
}

/* -------------------------------------------------------------------- init */

async function init() {
  const { body } = await api("/api/v1/config/schema");
  schema = body;
  buildForm();
  await refreshState();
  await refreshStatus();
  $("#apply").addEventListener("click", doApply);
  $("#reset").addEventListener("click", () => { dirty.clear(); fieldErrors.clear(); refreshForm(); });
  $("#mining-pause").addEventListener("click", () => setMiningState("paused"));
  $("#mining-resume").addEventListener("click", () => setMiningState("running"));
  setInterval(refreshStatus, 2000);
  setInterval(() => { if (!applying) refreshState(); }, 4000);
}

init();
