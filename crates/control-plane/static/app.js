/* Schema-driven control plane: the form comes from /api/v1/config/schema and
 * is populated from /api/v1/config; nothing here hard-codes individual settings
 * beyond the UI-only ignore rules below. */
"use strict";

const TOKEN = window.CONTROL_PLANE_TOKEN;
const $ = (sel) => document.querySelector(sel);

let schema = null;          // {settings: [{key, default, group, scope, control, options, optional, help, warning}], boot_settings: [{key, value, group, help}]}
let lastState = null;       // last /api/v1/config payload
let dirty = new Map();      // key -> edited value (string)
let fieldErrors = new Map(); // key -> latest client/server validation message
let applying = false;
let latestStatus = null;
let latestJobs = null;
let latestActiveJobId = null;
let selectedJobId = null;
let selectedJob = null;
let selectedJobEvents = [];
let selectedJobEventAfter = 0;
let renderedJobEventCount = 0;
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
let dashboardRefreshing = false;
let dashboardRefreshQueued = false;
let dashboardRefreshForceQueued = false;
let dashboardPollTimer = null;
const dashboardRenderCache = new Map();
const DASHBOARD_ACTIVE_POLL_MS = 1000;
const DEFAULT_DASHBOARD_IDLE_POLL_MS = 10000;
const DASHBOARD_IDLE_POLL_OPTIONS = [5000, 10000, 30000, 60000];
const DASHBOARD_IDLE_POLL_STORAGE_KEY = "simchain-dashboard-idle-poll-ms";
const MAX_SELECTED_JOB_EVENTS = 1000;
let dashboardIdlePollMs = loadDashboardIdlePollMs();
let activeDashboardTab = "overview";
let openHelpPopover = null;
let helpPopoverSeq = 0;

const GROUP_TITLES = {
  "mining": "Mining",
  "spam-basics": "Spam basics",
  "spam-advanced": "Spam advanced",
};

const REGTEST_TEST_MNEMONIC =
  "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const REGTEST_TEST_KEYS = Object.freeze([
  ["m/84'/1'/0'/0/0", "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk", "cTGhosGriPpuGA586jemcuH9pE9spwUmneMBmYYzrQEbY92DJrbo"],
  ["m/84'/1'/0'/0/1", "bcrt1qd7spv5q28348xl4myc8zmh983w5jx32cs707jh", "cQFUndrpAyMaE3HAsjMCXiT94MzfsABCREat1x7Qe3Mtq9KihD4V"],
  ["m/84'/1'/0'/0/2", "bcrt1qxdyjf6h5d6qxap4n2dap97q4j5ps6ua8jkxz0z", "cRe5KDj3rcZJAtZVmWe3G2rdGdXyKCjVbWBVDXqSg2WHq1qq6MNe"],
  ["m/84'/1'/0'/0/3", "bcrt1qynpgs6wap6h9uvy7j0xlesew2w82qn039tzepj", "cVeaVbQmeiwkYUqoWcmGJMHLppY8N1DtZqQ8tFUchhJZa6AKSMXd"],
  ["m/84'/1'/0'/0/4", "bcrt1q677973lw0w796gttpy52f296jqaaksz0kadvlr", "cVUnp4ArCgWBBNCnat7YeeCsoZPag3cKHhxYp9YLhfKS96MjLKzm"],
  ["m/84'/1'/0'/0/5", "bcrt1qr7scvm07ta0ldzlrmk7rnmc9lk356yarcts3za", "cVcKpznDBkA83HeX6beHD7D6EJ1XRkcP8mrHpnmMUzkgJwtTkppp"],
  ["m/84'/1'/0'/0/6", "bcrt1q4e9q5taxnsvc6m0uxv6h75mkzvnkxeqk6l90u2", "cNmkULcPZC4gXGVJTGNzNnw1Me1QpeEVnxY8qGBpKNwvaR5er4e1"],
  ["m/84'/1'/0'/0/7", "bcrt1qfsryn6hh2yhpxpp7m9dh54x89wettyfkhat7dd", "cVSKaLPiLMG5BDzsJDXSqqAzJedGjJTCVTdEKM8eZ65Pdo5v3UYY"],
  ["m/84'/1'/0'/0/8", "bcrt1qk9ca9jh7a2muk2venu26qsc2an5cvnwpmze5gq", "cTb6dS7GbsvRQdypGVmExBRV1bJq8Pn4fyx1SSdViksVLxnkKtPG"],
  ["m/84'/1'/0'/0/9", "bcrt1ql483wsftk62xvt4k5w608h2w9yy2nrnmskm03v", "cS1TKTLUB8i1BWhZax6M3AxfrjNUQzAKq2UVUz82mN4GP99r85FY"],
  ["m/84'/1'/0'/0/10", "bcrt1qz62u6t0px5tpyplrxuh2zyw6ycejyt9j0w3j4s", "cNUeAibgrGsMhZP9FUiihbfwNvnC8JLkaiwyzgwXSGseHXJvm4LA"],
]);

function dependencyIgnoredReason(key, values) {
  const spamEnabled = values.get("ENABLE_SPAM") === "true";
  const spamSetting = key.startsWith("SPAM_") || key === "ENABLE_SPAM_REPLACES";
  const fanoutAuto = values.get("SPAM_FANOUT_AUTO") === "true";
  const spamReplaces = values.get("ENABLE_SPAM_REPLACES") === "true";
  if (spamSetting && !spamEnabled) return "ignored: ENABLE_SPAM=false";
  if (key === "SPAM_FANOUT_UTXOS" && fanoutAuto) return "ignored: SPAM_FANOUT_AUTO=true";
  if (key === "SPAM_REPLACES_PER_MINER_PER_BLOCK" && !spamReplaces)
    return "ignored: ENABLE_SPAM_REPLACES=false";
  return null;
}

/* UI-only relevance rules: fields ignored by the current desired policy. */
function ignoredReason(key, values) {
  const dependencyReason = dependencyIgnoredReason(key, values);
  if (dependencyReason) return dependencyReason;
  const dataMode = Number(values.get("SPAM_TX_DATA_MAX_BYTES") || "0") > 0;
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

function snapshotKey(value) {
  return JSON.stringify(value ?? null);
}

function snapshotSectionChanged(name, value, force = false) {
  const key = snapshotKey(value);
  if (!force && dashboardRenderCache.get(name) === key) return false;
  dashboardRenderCache.set(name, key);
  return true;
}

function clearDashboardSection(name) {
  dashboardRenderCache.delete(name);
}

function loadDashboardIdlePollMs() {
  try {
    const stored = Number(localStorage.getItem(DASHBOARD_IDLE_POLL_STORAGE_KEY));
    if (DASHBOARD_IDLE_POLL_OPTIONS.includes(stored)) return stored;
  } catch (_) {}
  return DEFAULT_DASHBOARD_IDLE_POLL_MS;
}

function setDashboardIdlePollMs(milliseconds) {
  dashboardIdlePollMs = DASHBOARD_IDLE_POLL_OPTIONS.includes(milliseconds)
    ? milliseconds
    : DEFAULT_DASHBOARD_IDLE_POLL_MS;
  try {
    localStorage.setItem(DASHBOARD_IDLE_POLL_STORAGE_KEY, String(dashboardIdlePollMs));
  } catch (_) {}
}

function projectConfigForForm(config) {
  if (!config) return null;
  return {
    generation: config.generation,
    desired: config.desired,
    desired_valid: config.desired_valid,
    desired_errors: config.desired_errors || [],
    warnings: config.warnings || [],
    effective: config.effective,
    pending_apply: config.pending_apply || [],
  };
}

function fmtDurationCoarse(seconds) {
  if (seconds == null || !Number.isFinite(seconds)) return null;
  if (seconds < 60) return "<1m";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

function visibleComponentState(component) {
  if (!component) return null;
  return {
    reachable: component.reachable,
    status: component.status,
    phase: component.phase || null,
    effective_generation: component.effective_generation ?? null,
    uptime: fmtDurationCoarse(component.uptime_secs),
    last_error: component.last_error || null,
    desired_state: component.desired_state || null,
    effective_state: component.effective_state || null,
    observed_height: component.observed_height ?? null,
    next_scheduled_attempt_ms: component.next_scheduled_attempt_ms ?? null,
    active_lease_count: component.active_lease_count ?? null,
    cycle_phase: component.cycle_phase || null,
    accepted_transactions: component.accepted_transactions ?? null,
    last_cycle_duration_ms: component.last_cycle_duration_ms ?? null,
    reconciliation_pending: component.reconciliation_pending || false,
  };
}

function projectStatusForDashboard(status) {
  if (!status) return null;
  return {
    height: status.height ?? null,
    best_hash: status.best_hash || null,
    mempool: status.mempool || null,
    recent_blocks: status.recent_blocks || [],
    cadence: status.cadence || null,
    components: Object.fromEntries(
      Object.entries(status.components || {}).map(([name, component]) =>
        [name, visibleComponentState(component)]
      )
    ),
    active_operation: status.active_operation || null,
    impairments: status.impairments || [],
    explorer: status.explorer || null,
    last_error: status.last_error || null,
    rpc_error: status.rpc_error || null,
    component_error: status.component_error || null,
    slow_error: status.slow_error || null,
  };
}

function activeMutationId() {
  return latestActiveJobId;
}

function mutationBlockedMessage() {
  const jobId = activeMutationId();
  return jobId ? `mutation coordinator is held by ${jobId}` : null;
}

/* ------------------------------------------------------------------- tabs */

const DASHBOARD_TABS = ["overview", "control", "faucet"];

function selectTab(tab, updateHash = true) {
  if (!DASHBOARD_TABS.includes(tab)) tab = "overview";
  const changed = activeDashboardTab !== tab;
  activeDashboardTab = tab;
  for (const name of DASHBOARD_TABS) {
    const selected = name === tab;
    const button = $(`#tab-${name}`);
    const panel = $(`#panel-${name}`);
    button.setAttribute("aria-selected", selected ? "true" : "false");
    panel.hidden = !selected;
    panel.classList.toggle("active", selected);
  }
  if (updateHash && location.hash !== `#${tab}`) {
    history.replaceState(null, "", `#${tab}`);
  }
  if (changed && schema) {
    refreshDashboard({ force: true }).catch((error) => console.error(error));
  }
}

function initTabs() {
  for (const tab of DASHBOARD_TABS) {
    $(`#tab-${tab}`).addEventListener("click", () => selectTab(tab));
  }
  window.addEventListener("hashchange", () =>
    selectTab(location.hash.slice(1), false)
  );
  selectTab(location.hash.slice(1), false);
}

function labelTitleHost(label) {
  let title = label.querySelector(":scope > .label-title");
  if (title) return title;

  const textNode = Array.from(label.childNodes)
    .find((node) => node.nodeType === Node.TEXT_NODE && node.textContent.trim());
  title = document.createElement("span");
  title.className = "label-title";
  title.textContent = textNode ? textNode.textContent.trim() : "";
  if (textNode) {
    label.replaceChild(title, textNode);
  } else {
    label.prepend(title);
  }
  return title;
}

function closeHelpPopover() {
  if (!openHelpPopover) return;
  openHelpPopover.button.setAttribute("aria-expanded", "false");
  openHelpPopover.popover.hidden = true;
  openHelpPopover = null;
}

function positionHelpPopover(button, popover) {
  const gutter = 8;
  const gap = 6;
  const viewportWidth = document.documentElement.clientWidth;
  const viewportHeight = document.documentElement.clientHeight;
  const buttonRect = button.getBoundingClientRect();
  const popoverRect = popover.getBoundingClientRect();
  const left = Math.min(
    Math.max(gutter, buttonRect.left),
    Math.max(gutter, viewportWidth - popoverRect.width - gutter)
  );
  const below = buttonRect.bottom + gap;
  const above = buttonRect.top - popoverRect.height - gap;
  const top = below + popoverRect.height <= viewportHeight - gutter
    ? below
    : Math.max(gutter, above);
  popover.style.left = `${Math.round(left)}px`;
  popover.style.top = `${Math.round(top)}px`;
}

function attachHelpButton(target, helpText) {
  const text = String(helpText || "").trim();
  if (!text || target.dataset.helpAttached === "true") return;
  target.dataset.helpAttached = "true";

  const host = target.tagName === "LABEL" ? labelTitleHost(target) : target;
  host.classList.add("help-host");

  const button = document.createElement("button");
  button.type = "button";
  button.className = "help-button";
  button.textContent = "?";
  button.setAttribute("aria-label", "Show help");
  button.setAttribute("aria-expanded", "false");

  const popover = document.createElement("span");
  popover.id = `help-popover-${++helpPopoverSeq}`;
  popover.className = "help-popover";
  popover.hidden = true;
  popover.textContent = text;
  button.setAttribute("aria-controls", popover.id);

  button.addEventListener("click", (event) => {
    event.preventDefault();
    event.stopPropagation();
    const wasOpen = !popover.hidden;
    closeHelpPopover();
    if (!wasOpen) {
      popover.hidden = false;
      positionHelpPopover(button, popover);
      button.setAttribute("aria-expanded", "true");
      openHelpPopover = { button, popover };
    }
  });
  popover.addEventListener("click", (event) => event.stopPropagation());
  host.append(" ", button, popover);
}

function initHelp() {
  for (const target of document.querySelectorAll("[data-help]")) {
    attachHelpButton(target, target.dataset.help);
  }
  document.addEventListener("click", closeHelpPopover);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeHelpPopover();
  });
  document.addEventListener("scroll", closeHelpPopover, true);
  window.addEventListener("resize", closeHelpPopover);
}

/* ------------------------------------------------------------------ status */

function fmtBytes(n) {
  if (n == null) return "–";
  if (n > 1e6) return (n / 1e6).toFixed(2) + " MB";
  if (n > 1e3) return (n / 1e3).toFixed(1) + " kB";
  return n + " B";
}

function fmtNumber(n) {
  return n == null ? "–" : Number(n).toLocaleString("en-US");
}

function fmtSeconds(seconds) {
  if (seconds == null || !Number.isFinite(seconds)) return "–";
  return `${seconds.toFixed(1)}s`;
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

function renderConnectionStatus(s) {
  const stale = !s.last_updated_ms || (Date.now() - s.last_updated_ms) > 8000;
  const conn = $("#conn");
  conn.textContent = stale ? (s.last_error ? `stale: ${s.last_error}` : "stale / RPC unavailable")
                           : `live · height ${s.height ?? "?"}${s.last_error ? ` · warning: ${s.last_error}` : ""}`;
  conn.className = "conn " + (stale || s.last_error ? "stale" : "ok");
}

function statusTileSnapshot(s) {
  const lastBlock = (s.recent_blocks || [])[0] || null;
  const mempool = s.mempool ? {
    tx_count: s.mempool.tx_count,
    vbytes: s.mempool.vbytes,
  } : null;
  return {
    height: s.height ?? null,
    cadence: s.cadence || null,
    mempool,
    best_hash: s.best_hash || null,
    last_block_size: lastBlock ? lastBlock.size_bytes : null,
    last_block_weight: lastBlock ? lastBlock.weight : null,
  };
}

function statusServicesSnapshot(s) {
  return Object.fromEntries(
    Object.entries(s.components || {}).map(([name, component]) => [name, {
      reachable: component.reachable,
      status: component.status,
      phase: component.phase || null,
      effective_generation: component.effective_generation ?? null,
      uptime: fmtDurationCoarse(component.uptime_secs),
      last_error: component.last_error || null,
      observed_height: component.observed_height ?? null,
      active_lease_count: component.active_lease_count ?? null,
      cycle_phase: component.cycle_phase || null,
      last_cycle_duration_ms: component.last_cycle_duration_ms ?? null,
      reconciliation_pending: component.reconciliation_pending || false,
      spam_capacity: component.spam_capacity || null,
      reconciliation_count: component.reconciliation_count ?? null,
      last_reconciliation_reason: component.last_reconciliation_reason || null,
    }])
  );
}

function statusBlocksSnapshot(s) {
  return {
    recent_blocks: (s.recent_blocks || []).map((block) => ({
      height: block.height,
      hash: block.hash,
      delta_secs: block.delta_secs ?? null,
      tx_count: block.tx_count,
      size_bytes: block.size_bytes,
      weight: block.weight,
    })),
    explorer_url: s.explorer ? s.explorer.url : null,
    explorer_reachable: s.explorer ? s.explorer.reachable : null,
  };
}

function statusControlsSnapshot(s) {
  return {
    active: activeMutationId(),
    mining: visibleComponentState((s.components || {}).mining),
    spam: visibleComponentState((s.components || {}).spam),
    changing_mining: changingComponentState.mining,
    changing_spam: changingComponentState.spam,
  };
}

function statusNetworkSnapshot(s) {
  return {
    impairments: s.impairments || [],
    unavailable_agents: Object.entries(s.components || {})
      .filter(([name, component]) => name.startsWith("network-agent-") && !component.reachable)
      .map(([name]) => name.replace("network-agent-", "")),
  };
}

function renderStatus(s, options = {}) {
  latestStatus = s;
  const force = options.force === true;
  renderConnectionStatus(s);

  if (snapshotSectionChanged("status_tiles", statusTileSnapshot(s), force)) {
    const cadence = s.cadence ? `${s.cadence.mean_secs.toFixed(1)}s (n=${s.cadence.samples})` : "–";
    const mp = s.mempool;
    const lastBlock = (s.recent_blocks || [])[0];
    $("#tiles").innerHTML =
      tile("height", s.height ?? "–") +
      tile("cadence", cadence) +
      tile("mempool txs", mp ? mp.tx_count : "–") +
      tile("mempool size", mp ? fmtBytes(mp.vbytes) + " vB" : "–") +
      tile("best hash", s.best_hash ? s.best_hash.slice(0, 12) + "…" : "–") +
      tile("last block size", lastBlock ? fmtBytes(lastBlock.size_bytes) : "–") +
      tile("last block weight", lastBlock ? fmtNumber(lastBlock.weight) + " WU" : "–");
  }
  if (snapshotSectionChanged("status_explorer", s.explorer || null, force)) {
    renderExplorer(s.explorer);
  }

  if (snapshotSectionChanged("status_blocks", statusBlocksSnapshot(s), force)) {
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
  }

  if (snapshotSectionChanged("status_services", statusServicesSnapshot(s), force)) {
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
      const uptime = fmtDurationCoarse(component.uptime_secs);
      if (uptime) details.push(`up ${uptime}`);
      if (component.observed_height != null) details.push(`height ${component.observed_height}`);
      if (component.active_lease_count) details.push(`${component.active_lease_count} lease(s)`);
      if (component.cycle_phase) details.push(`cycle ${component.cycle_phase}`);
      if (name === "spam" && component.last_cycle_duration_ms != null) {
        details.push(`last cycle ${fmtSeconds(component.last_cycle_duration_ms / 1000)}`);
      }
      if (component.reconciliation_pending) details.push("reconciliation pending");
      if (name === "spam" && component.spam_capacity) {
        const capacity = component.spam_capacity;
        details.push(`capacity ${capacity.state}`);
        details.push(`branches ${capacity.usable_branches_per_miner}/${capacity.required_branches_per_miner}/${capacity.target_branches_per_miner}`);
        if (capacity.branch_provisioning) details.push("branch provisioning");
        if (capacity.floor_pool_provisioning) details.push("floor pool provisioning");
      }
      if (name === "spam" && component.reconciliation_count != null) {
        details.push(`recoveries ${component.reconciliation_count}`);
      }
      if (component.reachable) {
        text.textContent = `${name} · ${component.phase || component.status}` +
          (details.length ? ` · ${details.join(" · ")}` : "") +
          (component.last_error ? ` · ${component.last_error}` : "");
      } else {
        const lastKnown = [];
        if (component.phase && component.phase !== "unreachable") {
          lastKnown.push(`phase ${component.phase}`);
        }
        lastKnown.push(...details);
        text.textContent = `${name} · unreachable` +
          (lastKnown.length ? ` · last known: ${lastKnown.join(" · ")}` : "") +
          (component.last_error ? ` · ${component.last_error}` : "");
      }
      row.append(dot, text);
      services.append(row);
    }
  }

  if (snapshotSectionChanged("status_controls", statusControlsSnapshot(s), force)) {
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
      const lastCycle = spam.last_cycle_duration_ms == null
        ? "" : ` · last cycle ${fmtSeconds(spam.last_cycle_duration_ms / 1000)}`;
      const leases = spam.active_lease_count ? ` · ${spam.active_lease_count} job lease(s)` : "";
      spamState.textContent = `desired ${desired} · effective ${effective} · phase ${spam.phase || spam.status}${cycle}${accepted}${lastCycle}${leases}`;
      spamPause.disabled = changingComponentState.spam || desired === "paused" || activeMutationId() != null;
      spamResume.disabled = changingComponentState.spam || desired === "running" || activeMutationId() != null;
    }
  }

  if (snapshotSectionChanged("status_network", statusNetworkSnapshot(s), force)) {
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
  }
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

function unavailableSettingComponents() {
  if (!schema || !lastState) return new Set();
  const components = new Set(schema.settings.map((spec) => spec.component));
  return new Set([...components].filter((component) => {
    const effective = lastState.effective[component];
    return !effective || !effective.reachable;
  }));
}

function specsInDisplayOrder(specs) {
  const ordered = [...specs];
  const meanIndex = ordered.findIndex((spec) => spec.key === "BLOCK_INTERVAL_MEAN_SECS");
  const minIndex = ordered.findIndex((spec) => spec.key === "BLOCK_INTERVAL_MIN_SECS");
  if (meanIndex >= 0 && minIndex >= 0) {
    const [mean] = ordered.splice(meanIndex, 1);
    const updatedMinIndex = ordered.findIndex((spec) => spec.key === "BLOCK_INTERVAL_MIN_SECS");
    ordered.splice(updatedMinIndex + 1, 0, mean);
  }
  return ordered;
}

function buildForm() {
  const container = $("#form");
  container.replaceChildren();
  const groups = new Map();
  for (const spec of schema.settings) {
    if (!groups.has(spec.group)) groups.set(spec.group, []);
    groups.get(spec.group).push(spec);
  }
  for (const [group, groupedSpecs] of groups) {
    const specs = specsInDisplayOrder(groupedSpecs);
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
      const labelTitle = document.createElement("span");
      labelTitle.className = "label-title";
      labelTitle.textContent = spec.key;
      label.append(labelTitle);
      attachHelpButton(label, spec.help);

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
    for (const boot of bootSettingsFor(group)) {
      div.append(buildBootField(boot));
    }
    container.append(div);
  }
}

/* Boot-time settings (e.g. the nodes' -fallbackfee) are shown for context
   only: the control plane cannot change them, so they render as read-only
   labels instead of inputs. */
function bootSettingsFor(group) {
  return (schema.boot_settings || []).filter((boot) => boot.group === group);
}

function buildBootField(boot) {
  const field = document.createElement("div");
  field.className = "field boot";
  field.dataset.bootKey = boot.key;

  const label = document.createElement("label");
  const labelTitle = document.createElement("span");
  labelTitle.className = "label-title";
  labelTitle.textContent = boot.key;
  label.append(labelTitle);
  attachHelpButton(label, boot.help);

  const value = document.createElement("div");
  value.className = "bootvalue";
  value.textContent = boot.value;

  const note = document.createElement("div");
  note.className = "running";
  note.textContent = boot.note || "read-only";

  field.append(label, value, note);
  return field;
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
  const unavailableComponents = unavailableSettingComponents();
  for (const spec of schema.settings) {
    const field = document.querySelector(`.field[data-key="${spec.key}"]`);
    if (!field) continue;
    const input = field.querySelector("input, select");
    const isDirty = dirty.has(spec.key);
    if (document.activeElement !== input) {
      input.value = isDirty ? dirty.get(spec.key) : (lastState.desired[spec.key] ?? "");
    }
    field.classList.toggle("dirty", isDirty);

    const componentUnavailable = unavailableComponents.has(spec.component);
    const ignored = ignoredReason(spec.key, values);
    const reason = componentUnavailable
      ? `${spec.component} worker unavailable; setting temporarily disabled`
      : ignored;
    field.classList.toggle("ignored", ignored != null);
    field.classList.toggle("unavailable", componentUnavailable);
    field.title = reason || "";
    input.disabled = componentUnavailable || activeMutationId() != null ||
      dependencyIgnoredReason(spec.key, values) != null;

    const validationEl = field.querySelector(".field-error");
    let validation = fieldErrors.get(spec.key) || "";
    if (!validation && !input.checkValidity()) validation = input.validationMessage;
    validationEl.textContent = validation;
    field.classList.toggle("invalid", validation !== "");

    const runningEl = field.querySelector(".running");
    const effective = effectiveValueFor(spec);
    if (effective == null) {
      runningEl.textContent = componentUnavailable
        ? `effective: ${spec.component} unavailable`
        : "effective: –";
      runningEl.className = "running" + (componentUnavailable ? " unavailable" : "");
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
  const impact = impacted.size
    ? "pending apply: " + impacts.join(", ")
    : "desired and effective configuration match";
  $("#impact").textContent = unavailableComponents.size
    ? `${impact} · apply waits for ${[...unavailableComponents].join(", ")} recovery`
    : impact;
  const invalid = [...document.querySelectorAll("#form input, #form select")]
    .some((input) => !input.checkValidity()) || fieldErrors.size > 0;
  $("#apply").disabled = applying || activeMutationId() != null || invalid ||
    unavailableComponents.size > 0 ||
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

function buildDashboardRequest() {
  const params = new URLSearchParams();
  params.set("tab", activeDashboardTab);
  if ((activeDashboardTab === "control" || activeDashboardTab === "faucet") && selectedJobId) {
    params.set("selected_job_id", selectedJobId);
  }
  if (activeDashboardTab === "control" && selectedJobId && selectedJobEventAfter > 0) {
    params.set("events_after", String(selectedJobEventAfter));
  }
  if (activeDashboardTab === "control") params.set("event_limit", "200");
  if (activeDashboardTab === "faucet" && selectedFaucetTxid) {
    params.set("selected_faucet_txid", selectedFaucetTxid);
  }
  const query = params.toString();
  return {
    url: "/api/v1/dashboard" + (query ? `?${query}` : ""),
    tab: activeDashboardTab,
    selectedJobId,
    selectedFaucetTxid,
  };
}

function preferredSelectedJobId(jobs, selectedJobPayload) {
  if (!jobs) return selectedJobPayload ? selectedJobPayload.id : selectedJobId;
  if (jobs.active_job_id) return jobs.active_job_id;
  if (selectedJobPayload) return selectedJobPayload.id;
  const ids = new Set((jobs.jobs || []).map((job) => job.id));
  if (selectedJobId && ids.has(selectedJobId)) return selectedJobId;
  return (jobs.jobs && jobs.jobs.length > 0) ? jobs.jobs[0].id : null;
}

function trimSelectedJobEvents() {
  const trim = selectedJobEvents.length - MAX_SELECTED_JOB_EVENTS;
  if (trim <= 0) return;
  selectedJobEvents.splice(0, trim);
  renderedJobEventCount = Math.max(0, renderedJobEventCount - trim);
  const events = $("#job-events");
  for (let i = 0; i < trim && events.firstElementChild; i += 1) {
    events.firstElementChild.remove();
  }
}

function applySelectedJobEvents(eventsPayload) {
  if (!eventsPayload) return false;
  const events = eventsPayload.events || [];
  selectedJobEvents.push(...events);
  trimSelectedJobEvents();
  selectedJobEventAfter = Math.max(selectedJobEventAfter, eventsPayload.next_sequence || 0);
  return events.length > 0;
}

function applyDashboardSnapshot(body, options = {}) {
  if (!body) return;
  const force = options.force === true;
  const request = options.request || {};
  const previousActive = activeMutationId();
  latestActiveJobId = body.active_job_id || null;
  let needsFormRefresh = false;
  let jobsRendered = false;
  let statusRendered = false;
  let faucetRendered = false;
  let selectedJobNeedsRender = false;
  let selectedJobResetEvents = false;

  if (body.config && snapshotSectionChanged("config", projectConfigForForm(body.config), force)) {
    if (!applying) {
      lastState = body.config;
      needsFormRefresh = true;
    }
  }

  let jobsNeedRender = false;
  if (body.jobs && snapshotSectionChanged("jobs", body.jobs, force)) {
    jobsNeedRender = true;
  }
  if (body.jobs) {
    latestJobs = body.jobs;
    const nextSelectedJobId = preferredSelectedJobId(body.jobs, body.selected_job);
    if (selectedJobId !== nextSelectedJobId) {
      selectedJobId = nextSelectedJobId;
      selectedJob = null;
      selectedJobEvents = [];
      selectedJobEventAfter = 0;
      renderedJobEventCount = 0;
      clearDashboardSection("selected_job");
      clearDashboardSection("selected_job_events");
      jobsNeedRender = true;
      selectedJobNeedsRender = true;
      selectedJobResetEvents = true;
    }
    if (!selectedJobId && !selectedJob) selectedJobNeedsRender = true;
  }

  if (body.status) {
    latestStatus = body.status;
    renderConnectionStatus(body.status);
    if (snapshotSectionChanged("status", projectStatusForDashboard(body.status), force)) {
      renderStatus(body.status, { force });
      statusRendered = true;
      jobsNeedRender = true;
      renderFaucetControls();
    }
  }

  if (body.faucet && snapshotSectionChanged("faucet", body.faucet, force)) {
    renderFaucetStatus(body.faucet);
    faucetRendered = true;
  }

  if (request.selectedFaucetTxid) {
    const transfer = Object.prototype.hasOwnProperty.call(body, "selected_faucet_transfer")
      ? body.selected_faucet_transfer : null;
    if (snapshotSectionChanged("selected_faucet_transfer", transfer, force)) {
      selectedFaucetTransfer = transfer;
      renderFaucetTransfer();
      if (selectedFaucetTransfer && selectedFaucetTransfer.delivery_state === "confirmed") {
        $("#faucet-progress").hidden = true;
      }
    }
  }

  if (Object.prototype.hasOwnProperty.call(body, "selected_job") &&
      snapshotSectionChanged("selected_job", body.selected_job, force)) {
    selectedJob = body.selected_job || null;
    selectedJobNeedsRender = true;
  }
  if (body.selected_job_events &&
      snapshotSectionChanged("selected_job_events", body.selected_job_events, force)) {
    if (applySelectedJobEvents(body.selected_job_events)) renderJobEvents();
  }

  if (jobsNeedRender) {
    renderJobs();
    jobsRendered = true;
  }
  if (selectedJobNeedsRender) {
    const needsEventPlaceholder = renderedJobEventCount === 0 && selectedJobEvents.length === 0;
    renderSelectedJob({
      renderEvents: selectedJobResetEvents || needsEventPlaceholder,
      resetEvents: selectedJobResetEvents,
    });
  }

  const nextActive = activeMutationId();
  if (previousActive !== nextActive) {
    if (!statusRendered && latestStatus) renderStatus(latestStatus);
    if (!jobsRendered) renderJobs();
    if (!faucetRendered) renderFaucetControls();
    needsFormRefresh = true;
  }
  if (needsFormRefresh && !applying) refreshForm();
}

function dashboardPollDelay() {
  const activeJob = activeMutationId() != null;
  const selectedJobRunning = selectedJob && !isTerminalJob(selectedJob.state);
  const pendingLocalAction = applying || startingJob || startingScenario || abortingJob ||
    faucetSubmitting || Object.values(startingAction).some(Boolean) ||
    releasingCheckpoints.size > 0 || changingComponentState.mining || changingComponentState.spam;
  const pendingFaucet = faucetStatus && faucetStatus.pending_transfer;
  return (activeJob || selectedJobRunning || pendingLocalAction || pendingFaucet)
    ? DASHBOARD_ACTIVE_POLL_MS : dashboardIdlePollMs;
}

function scheduleDashboardPoll(delay = dashboardPollDelay()) {
  if (dashboardPollTimer != null) clearTimeout(dashboardPollTimer);
  if (document.hidden) {
    dashboardPollTimer = null;
    return;
  }
  dashboardPollTimer = setTimeout(() => {
    refreshDashboard().catch((error) => console.error(error));
  }, delay);
}

async function refreshDashboard(options = {}) {
  if (dashboardRefreshing) {
    dashboardRefreshQueued = true;
    dashboardRefreshForceQueued = dashboardRefreshForceQueued || options.force === true;
    return;
  }
  dashboardRefreshing = true;
  if (dashboardPollTimer != null) {
    clearTimeout(dashboardPollTimer);
    dashboardPollTimer = null;
  }
  const request = buildDashboardRequest();
  try {
    const { ok, body } = await api(request.url);
    if (ok && body) applyDashboardSnapshot(body, { ...options, request });
  } finally {
    dashboardRefreshing = false;
    const queued = dashboardRefreshQueued;
    const queuedForce = dashboardRefreshForceQueued;
    dashboardRefreshQueued = false;
    dashboardRefreshForceQueued = false;
    if (queued) {
      await refreshDashboard({ force: queuedForce, reschedule: options.reschedule });
    } else if (options.reschedule !== false) {
      scheduleDashboardPoll();
    }
  }
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
    await refreshDashboard({ force: true });
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

function setSecretVisibility(element, button, value, revealed) {
  element.textContent = revealed ? value : "******************";
  element.classList.toggle("revealed", revealed);
  button.textContent = revealed ? "Hide" : "Show";
  button.setAttribute("aria-pressed", revealed ? "true" : "false");
}

async function writeClipboard(value) {
  if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
    try {
      await navigator.clipboard.writeText(value);
      return;
    } catch (_) {
      // Sandboxed or permission-restricted browsers may expose the API but
      // reject it. Fall through to the selection-based local copy path.
    }
  }
  const fallback = document.createElement("textarea");
  fallback.value = value;
  fallback.setAttribute("readonly", "");
  fallback.style.position = "fixed";
  fallback.style.opacity = "0";
  document.body.append(fallback);
  fallback.select();
  const copied = document.execCommand("copy");
  fallback.remove();
  if (!copied) throw new Error("browser rejected clipboard access");
}

async function copyRegtestCredential(value, label, button) {
  const status = $("#regtest-copy-status");
  const original = button.textContent;
  try {
    await writeClipboard(value);
    status.textContent = `${label} copied`;
    status.className = "copy-status";
    button.textContent = "Copied";
  } catch (error) {
    status.textContent = `Could not copy ${label}: ${error.message || error}`;
    status.className = "copy-status err";
  }
  window.setTimeout(() => {
    button.textContent = original;
    if (status.textContent.includes(label)) status.textContent = "";
  }, 1800);
}

function regtestCredentialCell(value, label, secret = false) {
  const cell = document.createElement("div");
  cell.className = `regtest-key-cell${secret ? " regtest-private-cell" : ""}`;
  const code = document.createElement("code");
  code.className = `credential-value${secret ? " secret-value" : ""}`;
  if (!secret) {
    code.textContent = value;
    code.title = value;
  }
  if (secret) {
    const reveal = document.createElement("button");
    reveal.type = "button";
    reveal.className = "small secondary";
    reveal.setAttribute("aria-label", `Reveal ${label}`);
    reveal.addEventListener("click", () => {
      const revealed = reveal.getAttribute("aria-pressed") !== "true";
      setSecretVisibility(code, reveal, value, revealed);
    });
    setSecretVisibility(code, reveal, value, false);
    cell.append(code, reveal);
  } else {
    cell.append(code);
  }
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "small secondary";
  copy.textContent = "Copy";
  copy.setAttribute("aria-label", `Copy ${label}`);
  copy.addEventListener("click", () => copyRegtestCredential(value, label, copy));
  cell.append(copy);
  return cell;
}

function initRegtestWallet() {
  const mnemonic = $("#regtest-mnemonic");
  const reveal = $("#regtest-mnemonic-reveal");
  setSecretVisibility(mnemonic, reveal, REGTEST_TEST_MNEMONIC, false);
  reveal.addEventListener("click", () => {
    const revealed = reveal.getAttribute("aria-pressed") !== "true";
    setSecretVisibility(mnemonic, reveal, REGTEST_TEST_MNEMONIC, revealed);
  });
  $("#regtest-mnemonic-copy").addEventListener("click", (event) =>
    copyRegtestCredential(REGTEST_TEST_MNEMONIC, "mnemonic", event.currentTarget)
  );

  const body = $("#regtest-keys");
  for (const [path, address, privateKey] of REGTEST_TEST_KEYS) {
    const row = body.insertRow();
    const pathCell = row.insertCell();
    const pathCode = document.createElement("code");
    pathCode.textContent = path;
    pathCell.append(pathCode);
    row.insertCell().append(regtestCredentialCell(address, `${path} address`));
    row.insertCell().append(regtestCredentialCell(privateKey, `${path} private key`, true));
  }
}

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
  attachHelpButton(addressLabel, "Destination regtest address that receives faucet funds. Example: paste a bcrt1 address from the wallet or service you want to fund.");
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
  attachHelpButton(amountLabel, "BTC amount for this destination. Example: 0.5 sends 0.5 BTC; values may use up to 8 decimal places.");
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
  const unavailable = missingComponents(["mining", "spam", "node2", "node3"]);
  $("#faucet-add-output").disabled = count >= 100 || faucetSubmitting;
  for (const button of document.querySelectorAll(".faucet-remove-output")) {
    button.disabled = count === 1 || faucetSubmitting;
  }
  $("#faucet-review").disabled = faucetSubmitting || !faucetStatus || !faucetStatus.available ||
    faucetStatus.pending_transfer != null || activeMutationId() != null || unavailable.length > 0;
  $("#faucet-review").title = unavailable.length
    ? `Unavailable while ${unavailable.join(", ")} ${unavailable.length === 1 ? "is" : "are"} unreachable`
    : "";
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
    title.textContent = `Node: ${wallet.source} · Wallet: ${wallet.wallet_name}`;
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
  if (selectedFaucetTransfer && selectedFaucetTransfer.delivery_state === "confirmed") {
    progress.hidden = true;
    renderFaucetTransfer();
    return;
  }
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
    selectJobLocally(body.job_id);
  } catch (error) {
    result.textContent = error.message || String(error);
    result.className = "action-result err";
  } finally {
    faucetSubmitting = false;
    renderFaucetControls();
    await refreshDashboard({ force: true });
  }
}

function renderJobs() {
  const active = activeMutationId();
  const lock = $("#mutation-lock");
  lock.textContent = active
    ? `mutation coordinator held by ${active}; incompatible controls are disabled`
    : "mutation coordinator is idle";
  lock.className = "mutation-lock" + (active ? " busy" : "");

  const start = $("#reorg-start");
  const reorgUnavailable = actionDependencyReason("reorg");
  start.disabled = startingJob || active != null || !$("#reorg-form").checkValidity() ||
    reorgUnavailable !== "";
  start.title = reorgUnavailable;
  setActionDependencyMessage("reorg", reorgUnavailable);
  start.textContent = startingJob ? "Starting…" : "Start reorg";
  const scenarioStart = $("#scenario-start");
  scenarioStart.disabled = startingScenario || active != null || !$("#scenario-form").checkValidity();
  scenarioStart.textContent = startingScenario ? "Starting…" : "Start scenario";
  for (const [action, formId, buttonId, label] of [
    ["mine", "mine-form", "mine-start", "Mine"],
    ["burst", "burst-form", "burst-start", "Create tx burst"],
    ["partition", "partition-form", "partition-start", "Start partition"],
    ["degrade", "degrade-form", "degrade-start", "Start degradation"],
  ]) {
    const button = $("#" + buttonId);
    const unavailable = actionDependencyReason(action);
    button.disabled = startingAction[action] || active != null ||
      !$("#" + formId).checkValidity() || unavailable !== "";
    button.title = unavailable;
    setActionDependencyMessage(action, unavailable);
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
  if (latestStatus) renderStatusControlsOnly();
}

function missingComponents(names) {
  if (!latestStatus) return [];
  const components = latestStatus.components || {};
  return [...new Set(names)].filter((name) => !components[name] || !components[name].reachable);
}

function actionDependencies(action) {
  switch (action) {
    case "mine":
      return ["mining", $("#mine-node").value];
    case "burst":
      return ["spam", $("#burst-node").value];
    case "reorg":
      return ["mining", "spam", "node1", $("#reorg-node").value];
    case "partition": {
      const node = $("#partition-node").value;
      return ["mining", "spam", "node1", "node2", "node3", `network-agent-${node}`];
    }
    case "degrade":
      return [`network-agent-${$("#degrade-node").value}`];
    default:
      return [];
  }
}

function actionDependencyReason(action) {
  const unavailable = missingComponents(actionDependencies(action));
  if (unavailable.length === 0) return "";
  return `Unavailable while ${unavailable.join(", ")} ${unavailable.length === 1 ? "is" : "are"} unreachable`;
}

function setActionDependencyMessage(action, message) {
  const result = $(`#${action}-action-result`);
  if (!result) return;
  if (message) {
    if (result.dataset.dependencyBlocked !== "true") {
      result.dataset.previousText = result.textContent;
      result.dataset.previousClass = result.className;
    }
    result.dataset.dependencyBlocked = "true";
    result.textContent = message;
    result.className = "action-result err";
  } else if (result.dataset.dependencyBlocked === "true") {
    result.textContent = result.dataset.previousText || "";
    result.className = result.dataset.previousClass || "action-result";
    delete result.dataset.dependencyBlocked;
    delete result.dataset.previousText;
    delete result.dataset.previousClass;
  }
}

function renderStatusControlsOnly() {
  const active = activeMutationId() != null;
  for (const id of ["mining-pause", "mining-resume", "spam-pause", "spam-resume"]) {
    if (active) $("#" + id).disabled = true;
  }
}

function renderSelectedJob(options = {}) {
  const renderEvents = options.renderEvents !== false;
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
  if (renderEvents) renderJobEvents({ reset: options.resetEvents === true });
}

function renderJobEvents(options = {}) {
  const events = $("#job-events");
  if (options.reset || renderedJobEventCount > selectedJobEvents.length) {
    events.replaceChildren();
    renderedJobEventCount = 0;
  }
  if (renderedJobEventCount === 0 && events.firstElementChild &&
      events.firstElementChild.classList.contains("muted")) {
    events.replaceChildren();
  }
  for (const event of selectedJobEvents.slice(renderedJobEventCount)) {
    const item = document.createElement("li");
    const heading = document.createElement("span");
    heading.className = "event-heading";
    heading.textContent = `${event.sequence} · ${event.phase}`;
    const message = document.createElement("span");
    message.textContent = event.message;
    item.append(heading, message);
    events.append(item);
  }
  renderedJobEventCount = selectedJobEvents.length;
  if (selectedJobId && selectedJobEvents.length === 0) {
    events.replaceChildren();
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

function selectJobLocally(jobId) {
  if (selectedJobId !== jobId) {
    selectedJobId = jobId;
    selectedJob = null;
    selectedJobEvents = [];
    selectedJobEventAfter = 0;
    renderedJobEventCount = 0;
    clearDashboardSection("selected_job");
    clearDashboardSection("selected_job_events");
    renderJobs();
    renderSelectedJob({ resetEvents: true });
  }
}

async function selectJob(jobId) {
  selectJobLocally(jobId);
  await refreshDashboard({ force: true });
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
    selectJobLocally(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingJob = false;
    await refreshDashboard({ force: true });
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
    selectJobLocally(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingScenario = false;
    await refreshDashboard({ force: true });
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
    data_bytes: Number($("#burst-data-bytes").value),
  };
  result.textContent = `Submitting ${isMine ? "mine" : "tx burst"} job…`;
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
    selectJobLocally(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingAction[action] = false;
    await refreshDashboard({ force: true });
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
    selectJobLocally(body.job_id);
  } catch (error) {
    result.textContent = String(error);
    result.className = "action-result err";
  } finally {
    startingAction[action] = false;
    await refreshDashboard({ force: true });
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
    await refreshDashboard({ force: true });
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
    await refreshDashboard({ force: true });
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
    await refreshDashboard({ force: true });
  }
}

/* -------------------------------------------------------------------- init */

async function init() {
  initTabs();
  initRegtestWallet();
  const { body } = await api("/api/v1/config/schema");
  schema = body;
  buildForm();
  addFaucetOutput();
  initHelp();
  await refreshDashboard({ force: true, reschedule: false });
  const pollInterval = $("#poll-interval");
  pollInterval.value = String(dashboardIdlePollMs);
  pollInterval.addEventListener("change", () => {
    setDashboardIdlePollMs(Number(pollInterval.value));
    pollInterval.value = String(dashboardIdlePollMs);
    scheduleDashboardPoll();
  });
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
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      if (dashboardPollTimer != null) clearTimeout(dashboardPollTimer);
      dashboardPollTimer = null;
    } else {
      refreshDashboard({ force: true }).catch((error) => console.error(error));
    }
  });
  scheduleDashboardPoll();
}

init();
