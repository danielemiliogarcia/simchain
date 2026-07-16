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
let latestJobs = null;
let selectedJobId = null;
let selectedJob = null;
let selectedJobEvents = [];
let selectedJobEventAfter = 0;
let jobsRefreshing = false;
let startingJob = false;
let startingScenario = false;
const startingAction = { mine: false, burst: false, partition: false, degrade: false };
let abortingJob = false;
let faucetStatus = null;
let faucetSubmitting = false;
let reviewedFaucetRequest = null;
let selectedFaucetTxid = null;
let selectedFaucetTransfer = null;
const releasingCheckpoints = new Set();
const changingComponentState = { mining: false, spam: false };

const GROUP_TITLES = {
  "mining": "Mining",
  "spam-basics": "Spam basics",
  "spam-advanced": "Spam advanced",
};

/* UI-only relevance rules: fields ignored by the current desired policy. */
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

function activeMutationId() {
  return (latestJobs && latestJobs.active_job_id) ||
    (latestStatus && latestStatus.active_operation && latestStatus.active_operation.job_id) || null;
}

function mutationBlockedMessage() {
  const jobId = activeMutationId();
  return jobId ? `mutation coordinator is held by ${jobId}` : null;
}

/* ------------------------------------------------------------------ status */

function fmtBytes(n) {
  if (n == null) return "–";
  if (n > 1e6) return (n / 1e6).toFixed(2) + " MB";
  if (n > 1e3) return (n / 1e3).toFixed(1) + " kB";
  return n + " B";
}

function tile(k, v) {
  return `<div class="tile"><div class="k">${escapeHtml(k)}</div><div class="v">${escapeHtml(v)}</div></div>`;
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function httpUrl(value) {
  try {
    const url = new URL(value);
    return (url.protocol === "http:" || url.protocol === "https:") ? url : null;
  } catch (_) {
    return null;
  }
}

function explorerBlockUrl(explorer, hash) {
  const base = explorer && httpUrl(explorer.url);
  if (!base || !/^[0-9a-f]{64}$/i.test(hash)) return null;
  return new URL(`/block/${hash}`, base).toString();
}

function renderExplorer(explorer) {
  const container = $("#explorer-status");
  const dot = container.querySelector(".dot");
  const label = container.querySelector("span:not(.dot)");
  const link = $("#explorer-link");
  const url = explorer && httpUrl(explorer.url);
  dot.className = "dot " + (explorer && explorer.reachable ? "ok" : "warn");
  label.textContent = explorer
    ? `mempool.space ${explorer.reachable ? "reachable" : "unreachable"}`
    : "mempool.space unavailable";
  link.hidden = !url;
  if (url) link.href = url.toString();
  else link.removeAttribute("href");
  container.title = (explorer && explorer.error) || "";
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
  renderExplorer(s.explorer);

  const blockBody = $("#blocks tbody");
  blockBody.replaceChildren();
  for (const block of s.recent_blocks || []) {
    const row = blockBody.insertRow();
    const height = row.insertCell();
    const blockUrl = explorerBlockUrl(s.explorer, block.hash);
    if (blockUrl) {
      const link = document.createElement("a");
      link.href = blockUrl;
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      link.textContent = String(block.height);
      height.append(link);
    } else {
      height.textContent = String(block.height);
    }
    for (const value of [
      block.hash ? block.hash.slice(0, 10) + "…" : "–",
      block.delta_secs == null ? "–" : Math.max(0, block.delta_secs) + "s",
      block.tx_count,
      fmtBytes(block.size_bytes),
      block.weight,
    ]) {
      const cell = row.insertCell();
      cell.textContent = String(value);
    }
  }
  if (blockBody.rows.length === 0) {
    const cell = blockBody.insertRow().insertCell();
    cell.colSpan = 6;
    cell.textContent = "no blocks yet";
  }

  const max = Math.max(1, ...(s.fee_histogram || []).map((b) => b.count));
  $("#fees").innerHTML = (s.fee_histogram || []).map((b) =>
    `<div class="bar-row"><span class="lbl">${escapeHtml(b.label)}</span>` +
    `<div class="bar" style="width:${(100 * b.count / max).toFixed(1)}%"></div>` +
    `<span class="n">${escapeHtml(b.count)}</span></div>`).join("") || "–";

  const services = $("#services");
  services.replaceChildren();
  for (const [name, component] of Object.entries(s.components || {})) {
    const row = document.createElement("div");
    row.className = "svc";
    const dot = document.createElement("span");
    const warning = component.reachable && component.last_error;
    dot.className = "dot " + (!component.reachable ? "err" : warning ? "warn" : "ok");
    const text = document.createElement("span");
    const details = [];
    if (component.effective_generation != null) details.push(`gen ${component.effective_generation}`);
    if (component.uptime_secs != null) details.push(`up ${component.uptime_secs}s`);
    if (component.observed_height != null) details.push(`height ${component.observed_height}`);
    if (component.active_lease_count) details.push(`${component.active_lease_count} lease(s)`);
    if (component.cycle_phase) details.push(`cycle ${component.cycle_phase}`);
    if (component.reconciliation_pending) details.push("reconciliation pending");
    text.textContent = `${name} · ${component.phase || component.status}` +
      (details.length ? ` · ${details.join(" · ")}` : "") +
      (component.last_error ? ` · ${component.last_error}` : "");
    row.append(dot, text);
    services.append(row);
  }

  const mining = (s.components || {}).mining;
  const miningState = $("#mining-state");
  const pause = $("#mining-pause");
  const resume = $("#mining-resume");
  if (!mining || !mining.reachable) {
    miningState.textContent = mining && mining.last_error
      ? `mining worker unreachable: ${mining.last_error}` : "mining worker unavailable";
    pause.disabled = true;
    resume.disabled = true;
  } else {
    const desired = mining.desired_state || "unknown";
    const effective = mining.effective_state || "unknown";
    const next = mining.next_scheduled_attempt_ms == null
      ? "" : ` · next attempt ${new Date(mining.next_scheduled_attempt_ms).toLocaleTimeString()}`;
    const leases = mining.active_lease_count ? ` · ${mining.active_lease_count} job lease(s)` : "";
    miningState.textContent = `desired ${desired} · effective ${effective} · phase ${mining.phase || mining.status}${next}${leases}`;
    pause.disabled = changingComponentState.mining || desired === "paused" || activeMutationId() != null;
    resume.disabled = changingComponentState.mining || desired === "running" || activeMutationId() != null;
  }

  const spam = (s.components || {}).spam;
  const spamState = $("#spam-state");
  const spamPause = $("#spam-pause");
  const spamResume = $("#spam-resume");
  if (!spam || !spam.reachable) {
    spamState.textContent = spam && spam.last_error
      ? `spam worker unreachable: ${spam.last_error}` : "spam worker unavailable";
    spamPause.disabled = true;
    spamResume.disabled = true;
  } else {
    const desired = spam.desired_state || "unknown";
    const effective = spam.effective_state || "unknown";
    const cycle = spam.cycle_phase ? ` · cycle ${spam.cycle_phase}` : "";
    const accepted = spam.accepted_transactions == null
      ? "" : ` · accepted ${spam.accepted_transactions}`;
    const leases = spam.active_lease_count ? ` · ${spam.active_lease_count} job lease(s)` : "";
    spamState.textContent = `desired ${desired} · effective ${effective} · phase ${spam.phase || spam.status}${cycle}${accepted}${leases}`;
    spamPause.disabled = changingComponentState.spam || desired === "paused" || activeMutationId() != null;
    spamResume.disabled = changingComponentState.spam || desired === "running" || activeMutationId() != null;
  }
  const impairments = s.impairments || [];
  const unavailableAgents = Object.entries(s.components || {})
    .filter(([name, component]) => name.startsWith("network-agent-") && !component.reachable)
    .map(([name]) => name.replace("network-agent-", ""));
  const networkDetails = impairments.map((item) =>
    `${item.node}: ${item.kind} · owner ${item.owner_job_id}`);
  if (unavailableAgents.length) networkDetails.push(`agents unreachable: ${unavailableAgents.join(", ")}`);
  $("#network-status").textContent = networkDetails.length === 0
    ? "all P2P links clear"
    : networkDetails.join(" · ");
  refreshForm();
}

/* ---------------------------------------------------------------- settings */

function desiredValues() {
  const values = new Map(Object.entries(lastState ? lastState.desired : {}));
  for (const [k, v] of dirty) values.set(k, v);
  return values;
}

function effectiveValueFor(spec) {
  if (!lastState) return null;
  const svc = lastState.effective[spec.component];
  if (!svc || !svc.reachable || !svc.values) return null;
  return svc.values[spec.key] ?? "";
}

function buildForm() {
  const container = $("#form");
  container.replaceChildren();
  const groups = new Map();
  for (const spec of schema.settings) {
    if (!groups.has(spec.group)) groups.set(spec.group, []);
    groups.get(spec.group).push(spec);
  }
  for (const [group, specs] of groups) {
    const div = document.createElement("div");
    div.className = "group";
    const title = document.createElement("div");
    title.className = "gtitle";
    title.textContent = GROUP_TITLES[group] || group;
    div.append(title);
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
        for (const value of ["true", "false"]) {
          const option = document.createElement("option");
          option.value = value;
          option.textContent = value;
          input.append(option);
        }
      } else if (spec.control === "choice") {
        input = document.createElement("select");
        for (const value of spec.options || []) {
          const option = document.createElement("option");
          option.value = value;
          option.textContent = value;
          input.append(option);
        }
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
  const desired = lastState ? (lastState.desired[key] ?? "") : "";
  if (value === desired) dirty.delete(key); else dirty.set(key, value);
  fieldErrors.delete(key);
  refreshForm();
}

function refreshForm() {
  if (!schema || !lastState) return;
  const values = desiredValues();
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
    input.disabled = activeMutationId() != null ||
      (spec.key === "SPAM_FANOUT_UTXOS" && values.get("SPAM_FANOUT_AUTO") === "true");

    const validationEl = field.querySelector(".field-error");
    let validation = fieldErrors.get(spec.key) || "";
    if (!validation && !input.checkValidity()) validation = input.validationMessage;
    validationEl.textContent = validation;
    field.classList.toggle("invalid", validation !== "");

    const runningEl = field.querySelector(".running");
    const effective = effectiveValueFor(spec);
    if (effective == null) {
      runningEl.textContent = "effective: –";
      runningEl.className = "running";
    } else {
      runningEl.textContent = "effective: " + (effective === "" ? "(unset)" : effective);
      const differs = (lastState.desired[spec.key] ?? "") !== effective;
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
    return `${component} (${mode})`;
  });
  $("#impact").textContent = impacted.size
    ? "pending apply: " + impacts.join(", ")
    : "desired and effective configuration match";
  const invalid = [...document.querySelectorAll("#form input, #form select")]
    .some((input) => !input.checkValidity()) || fieldErrors.size > 0;
  $("#apply").disabled = applying || activeMutationId() != null || invalid ||
    (dirty.size === 0 && impacted.size === 0);

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

async function setComponentState(component, state) {
  if (changingComponentState[component]) return;
  const blocked = mutationBlockedMessage();
  if (blocked) {
    const result = $(`#${component}-action-result`);
    result.textContent = `${blocked}; open the active job below`;
    result.className = "action-result err";
    return;
  }
  changingComponentState[component] = true;
  renderStatus(latestStatus || { components: {} });
  const result = $(`#${component}-action-result`);
  result.textContent = `${state === "paused" ? "Pausing" : "Resuming"}…`;
  result.className = "action-result";
  try {
    const { ok, body } = await api(`/api/v1/${component}/state`, {
      method: "PUT",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
      },
      body: JSON.stringify({ state }),
    });
    result.textContent = ok
      ? `acknowledged at phase ${body.phase}`
      : ((body && body.error && body.error.message) || `${component} state change failed`);
    result.className = "action-result" + (ok ? "" : " err");
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    changingComponentState[component] = false;
    await refreshStatus();
    await refreshState();
  }
}

/* -------------------------------------------------------------------- jobs */

function isTerminalJob(state) {
  return ["succeeded", "failed", "aborted", "interrupted"].includes(state);
}

function formatJobTime(milliseconds) {
  return milliseconds == null ? "–" : new Date(milliseconds).toLocaleString();
}

/* ------------------------------------------------------------------ faucet */

const FAUCET_IDEMPOTENCY_STORAGE_KEY = "simchain-faucet-idempotency-key";
const FAUCET_PHASES = {
  faucet_preflight: "Checking faucet availability",
  acquiring_mining_lease: "Pausing mining safely",
  selecting_faucet_inputs: "Selecting mature miner funds",
  building_exact_zero_transaction: "Building and signing exact-zero transaction",
  arming_node2: "Arming node2",
  arming_node3: "Arming node3",
  submitting_node2: "Arming node2",
  submitting_node3: "Arming node3",
  verifying_next_block_priority: "Verifying next-block priority",
  armed_for_next_block: "Mining restored — waiting for next block",
};

function satsToBtc(sats) {
  const value = BigInt(sats || 0);
  const whole = value / 100000000n;
  const fraction = (value % 100000000n).toString().padStart(8, "0").replace(/0+$/, "");
  return fraction ? `${whole}.${fraction}` : String(whole);
}

function parseBtcSats(value) {
  const text = String(value).trim();
  const match = /^(0|[1-9][0-9]*)(?:\.([0-9]{1,8}))?$/.exec(text);
  if (!match) throw new Error("enter a positive BTC amount with at most 8 decimal places");
  const sats = BigInt(match[1]) * 100000000n + BigInt((match[2] || "").padEnd(8, "0") || "0");
  if (sats <= 0n || sats > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("amount is outside the supported range");
  }
  return Number(sats);
}

function validRegtestAddress(value) {
  const address = value.trim();
  return /^(bcrt1[ac-hj-np-z02-9]{8,}|[mn2][1-9A-HJ-NP-Za-km-z]{20,})$/.test(address);
}

function addFaucetOutput(address = "", amount = "1") {
  const outputs = $("#faucet-outputs");
  if (outputs.children.length >= 100) return;
  const row = document.createElement("div");
  row.className = "faucet-output-row";

  const addressLabel = document.createElement("label");
  addressLabel.textContent = "Regtest address";
  const addressInput = document.createElement("input");
  addressInput.type = "text";
  addressInput.autocomplete = "off";
  addressInput.spellcheck = false;
  addressInput.placeholder = "bcrt1q…";
  addressInput.value = address;
  addressInput.required = true;
  addressLabel.append(addressInput);

  const amountLabel = document.createElement("label");
  amountLabel.textContent = "Amount (BTC)";
  const amountInput = document.createElement("input");
  amountInput.type = "text";
  amountInput.inputMode = "decimal";
  amountInput.placeholder = "1.00000000";
  amountInput.value = amount;
  amountInput.required = true;
  amountLabel.append(amountInput);

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "small secondary faucet-remove-output";
  remove.textContent = "Remove";
  remove.addEventListener("click", () => {
    row.remove();
    renderFaucetControls();
  });
  for (const input of [addressInput, amountInput]) {
    input.addEventListener("input", () => {
      input.setCustomValidity("");
      renderFaucetControls();
    });
  }
  row.append(addressLabel, amountLabel, remove);
  outputs.append(row);
  renderFaucetControls();
}

function collectFaucetRequest() {
  const outputs = [];
  const seen = new Set();
  for (const row of $("#faucet-outputs").children) {
    const [addressInput, amountInput] = row.querySelectorAll("input");
    const address = addressInput.value.trim();
    if (!validRegtestAddress(address)) {
      addressInput.setCustomValidity("enter a valid regtest address");
      addressInput.reportValidity();
      throw new Error("invalid regtest destination");
    }
    addressInput.setCustomValidity("");
    if (seen.has(address)) {
      addressInput.setCustomValidity("destination addresses must be unique");
      addressInput.reportValidity();
      throw new Error("duplicate destination address");
    }
    seen.add(address);
    let amountSats;
    try {
      amountSats = parseBtcSats(amountInput.value);
      amountInput.setCustomValidity("");
    } catch (error) {
      amountInput.setCustomValidity(error.message);
      amountInput.reportValidity();
      throw error;
    }
    outputs.push({ address, amount_sats: amountSats });
  }
  if (outputs.length === 0) throw new Error("add at least one destination");
  const total = outputs.reduce((sum, output) => sum + BigInt(output.amount_sats), 0n);
  if (faucetStatus && total > BigInt(faucetStatus.max_request_sats)) {
    throw new Error(`request exceeds the ${satsToBtc(faucetStatus.max_request_sats)} BTC cap`);
  }
  return { source: $("#faucet-source").value, outputs, total_sats: total };
}

function renderFaucetControls() {
  const count = $("#faucet-outputs") ? $("#faucet-outputs").children.length : 0;
  if (!count) return;
  $("#faucet-add-output").disabled = count >= 100 || faucetSubmitting;
  for (const button of document.querySelectorAll(".faucet-remove-output")) {
    button.disabled = count === 1 || faucetSubmitting;
  }
  $("#faucet-review").disabled = faucetSubmitting || !faucetStatus || !faucetStatus.available ||
    faucetStatus.pending_transfer != null || activeMutationId() != null;
  $("#faucet-review").textContent = faucetSubmitting ? "Submitting…" : "Review transaction";
}

function renderFaucetStatus(status) {
  faucetStatus = status;
  const availability = $("#faucet-availability");
  if (status.available) {
    availability.textContent = `available · request cap ${satsToBtc(status.max_request_sats)} BTC · reserve ${satsToBtc(status.wallet_reserve_sats)} BTC per treasury`;
  } else {
    availability.textContent = `unavailable${status.last_probe_error ? ` · ${status.last_probe_error}` : ""}`;
  }
  const wallets = $("#faucet-wallets");
  wallets.replaceChildren();
  for (const wallet of status.wallets || []) {
    const item = document.createElement("div");
    const title = document.createElement("strong");
    title.textContent = `${wallet.source} · ${wallet.wallet_name}`;
    const balance = document.createElement("span");
    balance.textContent = wallet.error
      ? wallet.error
      : `${satsToBtc(wallet.eligible_confirmed_sats)} BTC confirmed · ${satsToBtc(wallet.available_after_reserve_sats)} BTC available after reserve`;
    item.append(title, balance);
    wallets.append(item);
  }
  if (status.pending_transfer) {
    selectedFaucetTxid = status.pending_transfer.txid;
    selectedFaucetTransfer = status.pending_transfer;
  } else if (!selectedFaucetTxid && status.recent_transfers && status.recent_transfers.length) {
    selectedFaucetTxid = status.recent_transfers[0].txid;
    selectedFaucetTransfer = status.recent_transfers[0];
  }
  renderFaucetTransfer();
  renderFaucetControls();
}

function transferExplorerUrl(transfer) {
  return transfer && httpUrl(transfer.explorer_url);
}

function renderFaucetTransfer() {
  const container = $("#faucet-transfer");
  const transfer = selectedFaucetTransfer;
  if (!transfer) {
    container.textContent = "no faucet transfer selected";
    container.className = "faucet-transfer muted";
    return;
  }
  container.replaceChildren();
  container.className = `faucet-transfer transfer-${transfer.delivery_state}`;
  const badge = document.createElement("strong");
  badge.className = "faucet-badge";
  badge.textContent = "SYSTEM FAUCET · 0 SAT FEE · MINER-PRIORITIZED";
  const state = document.createElement("div");
  state.className = "faucet-transfer-state";
  state.textContent = transfer.delivery_state === "armed"
    ? "armed in miner mempools; observer sees it after the block"
    : transfer.delivery_state === "confirmed"
      ? `confirmed in block ${transfer.confirmed_height}`
      : transfer.delivery_state.replaceAll("_", " ");
  const facts = document.createElement("div");
  facts.textContent = `${transfer.source} · ${transfer.outputs.length} destination(s) · ${satsToBtc(transfer.total_sats)} BTC · actual fee ${transfer.actual_fee_sats} sat · virtual delta ${satsToBtc(transfer.priority_delta_sats)} BTC · ${transfer.vsize} vB`;
  const txid = document.createElement("div");
  txid.className = "faucet-txid";
  txid.textContent = transfer.txid;
  const destinations = document.createElement("ul");
  for (const output of transfer.outputs) {
    const item = document.createElement("li");
    item.textContent = `${output.address} · ${satsToBtc(output.amount_sats)} BTC`;
    destinations.append(item);
  }
  container.append(badge, state, facts, txid, destinations);
  const explorer = transferExplorerUrl(transfer);
  if (explorer) {
    const link = document.createElement("a");
    link.href = explorer.toString();
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.textContent = "Open transaction in mempool.space ↗";
    container.append(link);
  }
  if (transfer.last_error) {
    const error = document.createElement("div");
    error.className = "faucet-transfer-error";
    error.textContent = transfer.last_error;
    container.append(error);
  }
}

function renderFaucetJob(job) {
  const progress = $("#faucet-progress");
  if (!job || job.kind !== "faucet") {
    progress.hidden = true;
    return;
  }
  progress.hidden = false;
  const result = job.result;
  if (result && result.txid &&
      (!selectedFaucetTransfer || selectedFaucetTransfer.txid !== result.txid ||
       selectedFaucetTransfer.delivery_state !== "confirmed")) {
    selectedFaucetTxid = result.txid;
    selectedFaucetTransfer = result;
  }
  const phase = result && result.delivery_state === "confirmed"
    ? `Confirmed in block ${result.confirmed_height}`
    : (FAUCET_PHASES[job.phase] || job.phase.replaceAll("_", " "));
  progress.textContent = `${phase} · job ${job.state}`;
  renderFaucetTransfer();
}

async function refreshFaucet() {
  const { ok, body } = await api("/api/v1/faucet");
  if (ok && body) renderFaucetStatus(body);
  if (selectedFaucetTxid) {
    const response = await api(`/api/v1/faucet/transfers/${encodeURIComponent(selectedFaucetTxid)}`);
    if (response.ok && response.body) {
      selectedFaucetTransfer = response.body;
      renderFaucetTransfer();
      if (selectedFaucetTransfer.delivery_state === "confirmed") {
        $("#faucet-progress").hidden = false;
        $("#faucet-progress").textContent = `Confirmed in block ${selectedFaucetTransfer.confirmed_height}`;
      }
    }
  }
}

function faucetIdempotencyKey() {
  let key = sessionStorage.getItem(FAUCET_IDEMPOTENCY_STORAGE_KEY);
  if (!key) {
    key = browserIdempotencyKey();
    sessionStorage.setItem(FAUCET_IDEMPOTENCY_STORAGE_KEY, key);
  }
  return key;
}

function reviewFaucet(event) {
  event.preventDefault();
  const result = $("#faucet-action-result");
  try {
    reviewedFaucetRequest = collectFaucetRequest();
    const summary = $("#faucet-confirm-summary");
    summary.textContent = `${reviewedFaucetRequest.outputs.length} destination(s) · ${satsToBtc(reviewedFaucetRequest.total_sats)} BTC total · source ${reviewedFaucetRequest.source}`;
    $("#faucet-confirm-dialog").showModal();
    result.textContent = "";
    result.className = "action-result";
  } catch (error) {
    result.textContent = error.message;
    result.className = "action-result err";
  }
}

async function submitFaucet(event) {
  event.preventDefault();
  if (!reviewedFaucetRequest || faucetSubmitting) return;
  $("#faucet-confirm-dialog").close();
  faucetSubmitting = true;
  renderFaucetControls();
  const result = $("#faucet-action-result");
  result.textContent = "Submitting durable faucet job…";
  result.className = "action-result";
  const request = {
    source: reviewedFaucetRequest.source,
    outputs: reviewedFaucetRequest.outputs,
  };
  try {
    const { ok, body } = await api("/api/v1/jobs/faucet", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
        "Idempotency-Key": faucetIdempotencyKey(),
      },
      body: JSON.stringify(request),
    });
    if (!ok) {
      sessionStorage.removeItem(FAUCET_IDEMPOTENCY_STORAGE_KEY);
      throw new Error((body && body.error && body.error.message) || "faucet request failed");
    }
    sessionStorage.removeItem(FAUCET_IDEMPOTENCY_STORAGE_KEY);
    result.textContent = `${body.reused ? "Reused" : "Started"} ${body.job_id}`;
    await selectJob(body.job_id);
  } catch (error) {
    result.textContent = error.message || String(error);
    result.className = "action-result err";
  } finally {
    faucetSubmitting = false;
    renderFaucetControls();
    await refreshFaucet();
    await refreshJobs();
  }
}

function renderJobs() {
  const active = latestJobs && latestJobs.active_job_id;
  const lock = $("#mutation-lock");
  lock.textContent = active
    ? `mutation coordinator held by ${active}; incompatible controls are disabled`
    : "mutation coordinator is idle";
  lock.className = "mutation-lock" + (active ? " busy" : "");

  const start = $("#reorg-start");
  start.disabled = startingJob || active != null || !$("#reorg-form").checkValidity();
  start.textContent = startingJob ? "Starting…" : "Start reorg";
  const scenarioStart = $("#scenario-start");
  scenarioStart.disabled = startingScenario || active != null || !$("#scenario-form").checkValidity();
  scenarioStart.textContent = startingScenario ? "Starting…" : "Start scenario";
  for (const [action, formId, buttonId, label] of [
    ["mine", "mine-form", "mine-start", "Mine"],
    ["burst", "burst-form", "burst-start", "Create burst"],
    ["partition", "partition-form", "partition-start", "Start partition"],
    ["degrade", "degrade-form", "degrade-start", "Start degradation"],
  ]) {
    const button = $("#" + buttonId);
    button.disabled = startingAction[action] || active != null || !$("#" + formId).checkValidity();
    button.textContent = startingAction[action] ? "Starting…" : label;
  }

  const tbody = $("#jobs tbody");
  tbody.replaceChildren();
  const jobs = (latestJobs && latestJobs.jobs) || [];
  if (jobs.length === 0) {
    const row = tbody.insertRow();
    const cell = row.insertCell();
    cell.colSpan = 5;
    cell.textContent = "no jobs yet";
  } else {
    for (const job of jobs) {
      const row = tbody.insertRow();
      if (job.id === selectedJobId) row.className = "selected";
      for (const value of [job.id, job.kind, job.state, job.phase]) {
        const cell = row.insertCell();
        cell.textContent = value;
      }
      const action = row.insertCell();
      const button = document.createElement("button");
      button.type = "button";
      button.className = "small secondary";
      button.textContent = job.id === selectedJobId ? "selected" : "view";
      button.disabled = job.id === selectedJobId;
      button.addEventListener("click", () => selectJob(job.id));
      action.append(button);
    }
  }
  refreshForm();
  if (latestStatus) renderStatusControlsOnly();
}

function renderStatusControlsOnly() {
  const active = activeMutationId() != null;
  for (const id of ["mining-pause", "mining-resume", "spam-pause", "spam-resume"]) {
    if (active) $("#" + id).disabled = true;
  }
}

function renderSelectedJob() {
  const detail = $("#job-detail");
  const abort = $("#job-abort");
  const download = $("#job-download");
  if (!selectedJob) {
    detail.textContent = selectedJobId ? "loading job…" : "no jobs yet";
    detail.className = "job-detail muted";
    abort.hidden = true;
    download.hidden = true;
  } else {
    const lines = [
      `${selectedJob.id} · ${selectedJob.kind} · ${selectedJob.state}`,
      `phase ${selectedJob.phase} · created ${formatJobTime(selectedJob.created_at_ms)}`,
    ];
    if (selectedJob.started_at_ms != null) lines.push(`started ${formatJobTime(selectedJob.started_at_ms)}`);
    if (selectedJob.ended_at_ms != null) lines.push(`ended ${formatJobTime(selectedJob.ended_at_ms)}`);
    if (selectedJob.failure) lines.push(`failure ${selectedJob.failure.code}: ${selectedJob.failure.message}`);
    if (selectedJob.current_step) {
      const step = selectedJob.current_step;
      lines.push(`step ${step.index}/${step.total} · ${step.kind} · ${step.state}`);
    }
    lines.push(`cleanup ${selectedJob.cleanup.state}`);
    for (const error of selectedJob.cleanup.errors || []) lines.push(`cleanup error: ${error}`);
    detail.textContent = lines.join("\n");
    detail.className = "job-detail state-" + selectedJob.state;
    abort.hidden = isTerminalJob(selectedJob.state);
    abort.disabled = abortingJob || selectedJob.state === "abort_requested";
    abort.textContent = abortingJob ? "Requesting…" :
      (selectedJob.state === "abort_requested" ? "Abort requested" : "Request abort");
    download.hidden = !isTerminalJob(selectedJob.state);
  }

  renderCheckpoints();
  renderFaucetJob(selectedJob);

  const events = $("#job-events");
  events.replaceChildren();
  for (const event of selectedJobEvents) {
    const item = document.createElement("li");
    const heading = document.createElement("span");
    heading.className = "event-heading";
    heading.textContent = `${event.sequence} · ${event.phase}`;
    const message = document.createElement("span");
    message.textContent = event.message;
    item.append(heading, message);
    events.append(item);
  }
  if (selectedJobId && selectedJobEvents.length === 0) {
    const item = document.createElement("li");
    item.className = "muted";
    item.textContent = "waiting for progress events…";
    events.append(item);
  }
}

function renderCheckpoints() {
  const container = $("#job-checkpoints");
  container.replaceChildren();
  const checkpoints = (selectedJob && selectedJob.checkpoints) || [];
  if (checkpoints.length === 0) return;
  const heading = document.createElement("h3");
  heading.textContent = "Checkpoints";
  container.append(heading);
  for (const checkpoint of checkpoints) {
    const row = document.createElement("div");
    row.className = `checkpoint checkpoint-${checkpoint.state}`;
    const summary = document.createElement("span");
    const arrival = checkpoint.arrived_at_ms == null
      ? "" : ` · ${formatJobTime(checkpoint.arrived_at_ms)}`;
    summary.textContent = `${checkpoint.name} · ${checkpoint.state} · generation ${checkpoint.generation}${arrival}`;
    row.append(summary);
    if (checkpoint.pause && checkpoint.state === "reached") {
      const release = document.createElement("button");
      release.type = "button";
      release.className = "small";
      release.textContent = releasingCheckpoints.has(checkpoint.name) ? "Releasing…" : "Release";
      release.disabled = releasingCheckpoints.has(checkpoint.name);
      release.addEventListener("click", () => releaseCheckpoint(checkpoint));
      row.append(release);
    }
    if (checkpoint.live_summary) {
      const live = document.createElement("pre");
      live.textContent = JSON.stringify(checkpoint.live_summary, null, 2);
      row.append(live);
    }
    container.append(row);
  }
}

async function selectJob(jobId) {
  if (selectedJobId !== jobId) {
    selectedJobId = jobId;
    selectedJob = null;
    selectedJobEvents = [];
    selectedJobEventAfter = 0;
    renderJobs();
    renderSelectedJob();
  }
  await refreshSelectedJob();
}

async function refreshSelectedJob() {
  const jobId = selectedJobId;
  if (!jobId) {
    renderSelectedJob();
    return;
  }
  const [detailResponse, eventResponse] = await Promise.all([
    api(`/api/v1/jobs/${encodeURIComponent(jobId)}`),
    api(`/api/v1/jobs/${encodeURIComponent(jobId)}/events?after=${selectedJobEventAfter}&limit=200`),
  ]);
  if (jobId !== selectedJobId) return;
  if (detailResponse.ok) selectedJob = detailResponse.body;
  if (eventResponse.ok) {
    selectedJobEvents.push(...eventResponse.body.events);
    selectedJobEventAfter = Math.max(selectedJobEventAfter, eventResponse.body.next_sequence);
  }
  renderSelectedJob();
}

async function refreshJobs() {
  if (jobsRefreshing) return;
  jobsRefreshing = true;
  try {
    const { ok, body } = await api("/api/v1/jobs");
    if (!ok || !body) return;
    latestJobs = body;
    if (body.active_job_id && selectedJobId !== body.active_job_id) {
      selectedJobId = body.active_job_id;
      selectedJob = null;
      selectedJobEvents = [];
      selectedJobEventAfter = 0;
    } else if (!selectedJobId && body.jobs.length > 0) {
      selectedJobId = body.jobs[0].id;
    }
    renderJobs();
    await refreshSelectedJob();
  } finally {
    jobsRefreshing = false;
  }
}

function browserIdempotencyKey() {
  if (globalThis.crypto && typeof globalThis.crypto.randomUUID === "function") {
    return `dashboard-${globalThis.crypto.randomUUID()}`;
  }
  return `dashboard-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

async function startReorg(event) {
  event.preventDefault();
  if (startingJob || activeMutationId() != null || !event.currentTarget.checkValidity()) return;
  startingJob = true;
  renderJobs();
  const result = $("#reorg-action-result");
  result.textContent = "Submitting durable reorg job…";
  result.className = "action-result";
  const request = {
    depth: Number($("#reorg-depth").value),
    empty: $("#reorg-empty").checked,
    node: $("#reorg-node").value,
    adds_new_txs: Number($("#reorg-adds").value),
    double_spend_pct: Number($("#reorg-double-spend").value),
  };
  try {
    const { ok, body } = await api("/api/v1/jobs/reorg", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
        "Idempotency-Key": browserIdempotencyKey(),
      },
      body: JSON.stringify(request),
    });
    if (!ok) throw new Error((body && body.error && body.error.message) || "reorg request failed");
    result.textContent = `${body.reused ? "Reused" : "Started"} ${body.job_id}`;
    result.className = "action-result";
    await selectJob(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingJob = false;
    await refreshJobs();
    await refreshStatus();
  }
}

async function startScenario(event) {
  event.preventDefault();
  if (startingScenario || activeMutationId() != null || !event.currentTarget.checkValidity()) return;
  startingScenario = true;
  renderJobs();
  const result = $("#scenario-action-result");
  result.textContent = "Validating and submitting durable scenario job…";
  result.className = "action-result";
  try {
    const { ok, body } = await api("/api/v1/jobs/scenario", {
      method: "POST",
      headers: {
        "Content-Type": "application/yaml",
        "Authorization": "Bearer " + TOKEN,
        "Idempotency-Key": browserIdempotencyKey(),
      },
      body: $("#scenario-yaml").value,
    });
    if (!ok) throw new Error((body && body.error && body.error.message) || "scenario request failed");
    result.textContent = `${body.reused ? "Reused" : "Started"} ${body.job_id}`;
    await selectJob(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingScenario = false;
    await refreshJobs();
    await refreshStatus();
  }
}

async function startBoundedAction(event, action) {
  event.preventDefault();
  if (startingAction[action] || activeMutationId() != null || !event.currentTarget.checkValidity()) return;
  startingAction[action] = true;
  renderJobs();
  const isMine = action === "mine";
  const result = $(isMine ? "#mine-action-result" : "#burst-action-result");
  const path = isMine ? "mine" : "spam-burst";
  const request = isMine ? {
    node: $("#mine-node").value,
    blocks: Number($("#mine-blocks").value),
  } : {
    node: $("#burst-node").value,
    txs: Number($("#burst-txs").value),
    outputs_per_tx: Number($("#burst-outputs").value),
  };
  result.textContent = `Submitting ${isMine ? "mine" : "spam burst"} job…`;
  result.className = "action-result";
  try {
    const { ok, body } = await api(`/api/v1/jobs/${path}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
        "Idempotency-Key": browserIdempotencyKey(),
      },
      body: JSON.stringify(request),
    });
    if (!ok) throw new Error((body && body.error && body.error.message) || `${path} request failed`);
    result.textContent = `${body.reused ? "Reused" : "Started"} ${body.job_id}`;
    await selectJob(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingAction[action] = false;
    await refreshJobs();
    await refreshStatus();
  }
}

async function startNetworkAction(event, action) {
  event.preventDefault();
  if (startingAction[action] || activeMutationId() != null || !event.currentTarget.checkValidity()) return;
  startingAction[action] = true;
  renderJobs();
  const partition = action === "partition";
  const result = $(`#${action}-action-result`);
  const request = partition ? {
    node: $("#partition-node").value,
    main_blocks: Number($("#partition-main-blocks").value),
    isolated_blocks: Number($("#partition-isolated-blocks").value),
  } : {
    node: $("#degrade-node").value,
    delay_ms: Number($("#degrade-delay").value),
    loss_pct: Number($("#degrade-loss").value),
    seconds: Number($("#degrade-seconds").value),
  };
  result.textContent = `Submitting ${partition ? "partition" : "degradation"} job…`;
  result.className = "action-result";
  try {
    const { ok, body } = await api(`/api/v1/jobs/${action}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "Authorization": "Bearer " + TOKEN,
        "Idempotency-Key": browserIdempotencyKey(),
      },
      body: JSON.stringify(request),
    });
    if (!ok) throw new Error((body && body.error && body.error.message) || `${action} request failed`);
    result.textContent = `${body.reused ? "Reused" : "Started"} ${body.job_id}`;
    await selectJob(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingAction[action] = false;
    await refreshJobs();
    await refreshStatus();
  }
}

async function releaseCheckpoint(checkpoint) {
  if (!selectedJobId || releasingCheckpoints.has(checkpoint.name)) return;
  releasingCheckpoints.add(checkpoint.name);
  renderSelectedJob();
  const result = $("#scenario-action-result");
  try {
    const { ok, body } = await api(
      `/api/v1/jobs/${encodeURIComponent(selectedJobId)}/checkpoints/${encodeURIComponent(checkpoint.name)}/release`,
      {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Authorization": "Bearer " + TOKEN,
        },
        body: JSON.stringify({ generation: checkpoint.generation }),
      });
    if (!ok) throw new Error((body && body.error && body.error.message) || "checkpoint release failed");
    result.textContent = `Released ${checkpoint.name} at generation ${checkpoint.generation}`;
    result.className = "action-result";
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    releasingCheckpoints.delete(checkpoint.name);
    await refreshJobs();
  }
}

function downloadSelectedJob() {
  if (!selectedJob) return;
  const artifact = JSON.stringify({ job: selectedJob, events: selectedJobEvents }, null, 2) + "\n";
  const url = URL.createObjectURL(new Blob([artifact], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `${selectedJob.id}.json`;
  link.click();
  URL.revokeObjectURL(url);
}

async function abortSelectedJob() {
  if (!selectedJobId || abortingJob) return;
  abortingJob = true;
  renderSelectedJob();
  const result = $("#reorg-action-result");
  try {
    const { ok, body } = await api(`/api/v1/jobs/${encodeURIComponent(selectedJobId)}/abort`, {
      method: "POST",
      headers: { "Authorization": "Bearer " + TOKEN },
    });
    if (!ok) throw new Error((body && body.error && body.error.message) || "abort request failed");
    result.textContent = `Abort requested for ${body.job_id}; waiting for a safe boundary and cleanup`;
    result.className = "action-result";
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    abortingJob = false;
    await refreshJobs();
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
  const blocked = mutationBlockedMessage();
  if (blocked) {
    showResult(`${blocked}; wait for its cleanup before changing configuration`, true);
    return;
  }
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
  addFaucetOutput();
  await refreshFaucet();
  await refreshJobs();
  $("#apply").addEventListener("click", doApply);
  $("#reset").addEventListener("click", () => { dirty.clear(); fieldErrors.clear(); refreshForm(); });
  $("#mining-pause").addEventListener("click", () => setComponentState("mining", "paused"));
  $("#mining-resume").addEventListener("click", () => setComponentState("mining", "running"));
  $("#spam-pause").addEventListener("click", () => setComponentState("spam", "paused"));
  $("#spam-resume").addEventListener("click", () => setComponentState("spam", "running"));
  $("#reorg-form").addEventListener("submit", startReorg);
  $("#reorg-form").addEventListener("input", renderJobs);
  $("#scenario-form").addEventListener("submit", startScenario);
  $("#scenario-form").addEventListener("input", renderJobs);
  $("#scenario-file").addEventListener("change", async (event) => {
    const file = event.target.files && event.target.files[0];
    if (!file) return;
    $("#scenario-yaml").value = await file.text();
    renderJobs();
  });
  $("#job-abort").addEventListener("click", abortSelectedJob);
  $("#job-download").addEventListener("click", downloadSelectedJob);
  $("#mine-form").addEventListener("submit", (event) => startBoundedAction(event, "mine"));
  $("#mine-form").addEventListener("input", renderJobs);
  $("#burst-form").addEventListener("submit", (event) => startBoundedAction(event, "burst"));
  $("#burst-form").addEventListener("input", renderJobs);
  $("#partition-form").addEventListener("submit", (event) => startNetworkAction(event, "partition"));
  $("#partition-form").addEventListener("input", renderJobs);
  $("#degrade-form").addEventListener("submit", (event) => startNetworkAction(event, "degrade"));
  $("#degrade-form").addEventListener("input", renderJobs);
  $("#faucet-form").addEventListener("submit", reviewFaucet);
  $("#faucet-add-output").addEventListener("click", () => addFaucetOutput("", "1"));
  $("#faucet-source").addEventListener("change", renderFaucetControls);
  $("#faucet-confirm").addEventListener("click", submitFaucet);
  setInterval(refreshStatus, 2000);
  setInterval(refreshFaucet, 2000);
  setInterval(() => { if (!applying) refreshState(); }, 4000);
  setInterval(refreshJobs, 1000);
}

init();
