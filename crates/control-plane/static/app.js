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
const releasingCheckpoints = new Set();
const changingComponentState = { mining: false, spam: false };

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
    const leases = mining.active_lease_count ? ` · ${mining.active_lease_count} job lease(s)` : "";
    miningState.textContent = `desired ${desired} · effective ${effective} · phase ${mining.phase || mining.status}${next}${leases}`;
    pause.disabled = changingComponentState.mining || desired === "paused" || activeMutationId() != null;
    resume.disabled = changingComponentState.mining || desired === "running" || activeMutationId() != null;
  }

  const spam = (s.components || {})["btc-simnet-spammer"];
  const spamState = $("#spam-state");
  const spamPause = $("#spam-pause");
  const spamResume = $("#spam-resume");
  if (!spam || !spam.present) {
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
  $("#network-status").textContent = impairments.length === 0
    ? "all P2P links clear"
    : impairments.map((item) => `${item.node}: ${item.kind} · owner ${item.owner_job_id}`).join(" · ");
  refreshForm();
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
    input.disabled = activeMutationId() != null ||
      (spec.key === "SPAM_FANOUT_UTXOS" && values.get("SPAM_FANOUT_AUTO") === "true");

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
  setInterval(refreshStatus, 2000);
  setInterval(() => { if (!applying) refreshState(); }, 4000);
  setInterval(refreshJobs, 1000);
}

init();
