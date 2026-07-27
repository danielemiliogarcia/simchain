// Static demo shim for the Simchain control dashboard.
//
// The published demo serves the REAL dashboard assets (index.html, styles.css,
// app.js) copied straight out of crates/control-plane/static/ by the Pages
// workflow, so it can never drift from the shipped UI. There is no backend
// behind it, so this file installs a fake one: it replaces window.fetch before
// app.js loads, answers the handful of GET endpoints the dashboard polls with
// canned data, and refuses every mutation with an explanation.
//
// Fixture shapes were captured from the real API responses, so the dashboard
// renders exactly as it does against a live stack.

(function () {
  "use strict";

  const REPO = "https://github.com/danielemiliogarcia/simchain";
  const START_HEIGHT = 1042;
  const BOOT_MS = Date.now();

  // ---------------------------------------------------------------- fixtures

  const CONFIG = {
    generation: 7,
    desired_valid: true,
    components: {},
    desired: {
      BLOCK_INTERVAL_MODE: "poisson",
      BLOCK_INTERVAL_MEAN_SECS: "8",
      BLOCK_INTERVAL_MIN_SECS: "5",
      BLOCK_INTERVAL_MAX_SECS: "12",
      MINER_WEIGHTS: "",
      MINING_RNG_SEED: "",
      ENABLE_SPAM: "true",
      ENABLE_SPAM_REPLACES: "false",
      SPAM_FANOUT_AUTO: "true",
      SPAM_FANOUT_UTXOS: "50",
      SPAM_FEE: "0.0001",
      SPAM_FILL_BLOCK_RATIO: "3",
      SPAM_FIXED_TXS_PER_BLOCK: "100",
      SPAM_FLOOR_POOL_TXS: "4000",
      SPAM_REPLACES_PER_MINER_PER_BLOCK: "5",
      SPAM_SENDMANY_OUTPUTS: "0",
      SPAM_SMALL_TXS_PER_BLOCK: "0",
      SPAM_TX_DATA_MAX_BYTES: "90000",
      SPAM_TX_DATA_MIN_BYTES: "250",
    },
  };
  CONFIG.effective = {
    mining: {
      generation: 7,
      reachable: true,
      values: {
        BLOCK_INTERVAL_MODE: CONFIG.desired.BLOCK_INTERVAL_MODE,
        BLOCK_INTERVAL_MEAN_SECS: CONFIG.desired.BLOCK_INTERVAL_MEAN_SECS,
        BLOCK_INTERVAL_MIN_SECS: CONFIG.desired.BLOCK_INTERVAL_MIN_SECS,
        BLOCK_INTERVAL_MAX_SECS: CONFIG.desired.BLOCK_INTERVAL_MAX_SECS,
        MINER_WEIGHTS: "",
        MINING_RNG_SEED: "",
      },
    },
    spam: {
      generation: 7,
      reachable: true,
      values: {
        ENABLE_SPAM: "true",
        ENABLE_SPAM_REPLACES: "false",
        SPAM_FANOUT_AUTO: "true",
        SPAM_FANOUT_UTXOS: "50",
        SPAM_FEE: "0.0001",
        SPAM_FILL_BLOCK_RATIO: "3",
        SPAM_FIXED_TXS_PER_BLOCK: "100",
        SPAM_FLOOR_POOL_TXS: "4000",
        SPAM_REPLACES_PER_MINER_PER_BLOCK: "5",
        SPAM_SENDMANY_OUTPUTS: "0",
        SPAM_SMALL_TXS_PER_BLOCK: "0",
        SPAM_TX_DATA_MAX_BYTES: "90000",
        SPAM_TX_DATA_MIN_BYTES: "250",
      },
    },
  };

  const FAUCET = {
    available: true,
    max_outputs: 100,
    max_request_sats: 10000000000,
    max_tx_vbytes: 10000,
    priority_delta_sats: 10000000000,
    wallet_reserve_sats: 60000000000,
    recent_transfers: [],
    wallets: [
      {
        source: "node2",
        wallet_name: "node2",
        eligible_confirmed_sats: 152500000000,
        available_after_reserve_sats: 92500000000,
      },
      {
        source: "node3",
        wallet_name: "node3",
        eligible_confirmed_sats: 148000000000,
        available_after_reserve_sats: 88000000000,
      },
    ],
  };

  // Deterministic pseudo-hash so repeated renders stay stable per height.
  function hashFor(height) {
    let h = (height * 2654435761) >>> 0;
    let out = "";
    for (let i = 0; i < 8; i += 1) {
      h = (h * 1664525 + 1013904223) >>> 0;
      out += h.toString(16).padStart(8, "0");
    }
    return "0000" + out.slice(0, 60);
  }

  function currentHeight() {
    return START_HEIGHT + Math.floor((Date.now() - BOOT_MS) / 8000);
  }

  function recentBlocks(tip) {
    const blocks = [];
    for (let i = 0; i < 12; i += 1) {
      const height = tip - i;
      const spread = [8, 11, 6, 9, 14, 7, 10, 5, 12, 8, 9, 7][i % 12];
      blocks.push({
        height,
        hash: hashFor(height),
        time: Math.floor(Date.now() / 1000) - i * 8,
        delta_secs: i === 11 ? null : spread,
        tx_count: 180 + ((height * 7) % 90),
        size_bytes: 920000 + ((height * 131) % 60000),
        weight: 3720000 + ((height * 517) % 200000),
        median_fee_rate_sat_vb: 2 + ((height % 9) * 0.5),
      });
    }
    return blocks;
  }

  function statusPayload() {
    const tip = currentHeight();
    return {
      height: tip,
      best_hash: hashFor(tip),
      mempool: {
        tx_count: 5200 + ((tip * 13) % 900),
        vbytes: 2850000 + ((tip * 37) % 200000),
        usage_bytes: 14500000 + ((tip * 53) % 900000),
        min_fee: 0.00001,
        min_relay_fee: 0.00001,
      },
      recent_blocks: recentBlocks(tip),
      cadence: { mean_secs: 8.2, samples: 12 },
      fee_histogram: [
        { label: "1-2", count: 1840 },
        { label: "2-4", count: 1520 },
        { label: "4-8", count: 980 },
        { label: "8-16", count: 540 },
        { label: "16+", count: 320 },
      ],
      components: {
        node1: { reachable: true, status: "ok", observed_height: tip },
        node2: { reachable: true, status: "ok", observed_height: tip },
        node3: { reachable: true, status: "ok", observed_height: tip },
        mining: {
          reachable: true,
          status: "running",
          phase: "running",
          effective_generation: 7,
          uptime_secs: Math.floor((Date.now() - BOOT_MS) / 1000) + 3600,
          desired_state: "running",
          effective_state: "running",
          observed_height: tip,
          active_lease_count: 0,
          last_mined_block: { node: "node2", height: tip, hash: hashFor(tip) },
        },
        spam: {
          reachable: true,
          status: "running",
          phase: "running",
          effective_generation: 7,
          uptime_secs: Math.floor((Date.now() - BOOT_MS) / 1000) + 3600,
          desired_state: "running",
          effective_state: "running",
          observed_height: tip,
          active_lease_count: 0,
          cycle_phase: "filling",
          accepted_transactions: 128400 + tip * 3,
          last_cycle_duration_ms: 1180,
          reconciliation_pending: false,
          reconciliation_count: 4,
        },
        "network-agent-node1": { reachable: true, status: "clear", phase: "clear", effective_generation: 0, uptime_secs: 7200, active_lease_count: 0 },
        "network-agent-node2": { reachable: true, status: "clear", phase: "clear", effective_generation: 0, uptime_secs: 7200, active_lease_count: 0 },
        "network-agent-node3": { reachable: true, status: "clear", phase: "clear", effective_generation: 0, uptime_secs: 7200, active_lease_count: 0 },
      },
      impairments: [],
      active_operations: [],
      desired_generation: 7,
      effective_generations: { mining: 7, spam: 7 },
      explorer: {
        url: "http://127.0.0.1:1080",
        reachable: true,
        indexer_reachable: true,
        synchronized: true,
        indexed_height: tip,
        indexed_hash: hashFor(tip),
      },
      last_updated_ms: Date.now(),
      slow_last_updated_ms: Date.now(),
    };
  }

  const JOBS = {
    jobs: [
      {
        job_id: "demo-reorg-1",
        kind: "reorg",
        state: "succeeded",
        phase: "completed",
        created_at_ms: BOOT_MS - 600000,
        updated_at_ms: BOOT_MS - 570000,
      },
      {
        job_id: "demo-partition-1",
        kind: "partition",
        state: "succeeded",
        phase: "completed",
        created_at_ms: BOOT_MS - 1500000,
        updated_at_ms: BOOT_MS - 1440000,
      },
    ],
  };

  function dashboardPayload() {
    return {
      status: statusPayload(),
      config: CONFIG,
      faucet: FAUCET,
      jobs: JOBS,
      user_address: {
        address: "bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk",
        explorer_url:
          "http://127.0.0.1:1080/address/bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk",
      },
    };
  }

  // ------------------------------------------------------------- the notice

  let noticeTimer = null;

  function showNotice(what) {
    let el = document.getElementById("simchain-demo-notice");
    if (!el) {
      el = document.createElement("div");
      el.id = "simchain-demo-notice";
      el.setAttribute("role", "status");
      el.style.cssText = [
        "position:fixed", "left:50%", "bottom:1.5rem", "transform:translateX(-50%)",
        "z-index:9999", "max-width:min(560px,92vw)", "padding:.9rem 1.1rem",
        "border-radius:10px", "border:1px solid #f7931a",
        "background:#1f232d", "color:#e6e9ef", "box-shadow:0 8px 30px rgba(0,0,0,.45)",
        "font:14px/1.5 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif",
      ].join(";");
      document.body.appendChild(el);
    }
    el.innerHTML =
      '<b style="color:#f7931a">This is a read-only preview.</b><br>' +
      "Nothing is running behind this page, so <b>" + what + "</b> cannot be " +
      "carried out. The numbers you see are fixed sample data. " +
      'To use the real thing, run the stack locally &mdash; ' +
      '<a href="' + REPO + '#quickstart" style="color:#4fc3f7">two commands in the Quickstart</a>.';
    el.style.display = "block";
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = setTimeout(function () { el.style.display = "none"; }, 9000);
  }

  function banner() {
    const bar = document.createElement("div");
    bar.style.cssText = [
      "position:sticky", "top:0", "z-index:9998", "padding:.55rem 1rem",
      "background:#f7931a", "color:#101010", "text-align:center",
      "font:600 13.5px/1.45 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif",
    ].join(";");
    bar.innerHTML =
      "Preview of the Simchain control dashboard &mdash; sample data, controls disabled. " +
      '<a href="' + REPO + '#quickstart" style="color:#101010">Run the real one locally</a> · ' +
      '<a href="./walkthrough.html" style="color:#101010">Feature walkthrough</a>';
    document.body.insertBefore(bar, document.body.firstChild);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", banner);
  } else {
    banner();
  }

  // ------------------------------------------------------------- fetch stub

  function json(body, status) {
    return Promise.resolve(
      new Response(JSON.stringify(body), {
        status: status || 200,
        headers: { "Content-Type": "application/json" },
      })
    );
  }

  // Human-readable name for whatever the dashboard just tried to do.
  function describe(path, method) {
    if (method === "PATCH" && path.startsWith("/api/v1/config")) return "changing settings";
    if (path.indexOf("/state") !== -1) return "pausing or resuming a worker";
    if (path.indexOf("/jobs/faucet") !== -1) return "sending faucet funds";
    if (path.indexOf("/jobs/reorg") !== -1) return "starting a reorg";
    if (path.indexOf("/jobs/rewind") !== -1) return "rewinding the chain";
    if (path.indexOf("/jobs/scenario") !== -1) return "running a scenario";
    if (path.indexOf("/abort") !== -1) return "aborting a job";
    if (path.indexOf("/jobs/") !== -1) return "starting that job";
    return "that action";
  }

  const realFetch = window.fetch ? window.fetch.bind(window) : null;

  window.fetch = function (input, init) {
    const url = typeof input === "string" ? input : (input && input.url) || "";
    const method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
    let path = url;
    try {
      path = new URL(url, window.location.href).pathname;
    } catch (_) { /* already a path */ }

    if (path.indexOf("/api/v1/") !== 0) {
      return realFetch ? realFetch(input, init) : json({}, 404);
    }

    if (method !== "GET") {
      showNotice(describe(path, method));
      return json(
        {
          error: {
            code: "component_unavailable",
            message:
              "This is a static preview with no control plane behind it. " +
              "Run Simchain locally to perform this action.",
          },
        },
        503
      );
    }

    if (path === "/api/v1/dashboard") return json(dashboardPayload());
    if (path === "/api/v1/config") return json(CONFIG);
    if (path === "/api/v1/config/schema") return json(window.SIMCHAIN_DEMO_SCHEMA || { settings: [] });
    if (path === "/api/v1/status") return json(statusPayload());
    if (path === "/api/v1/jobs") return json(JOBS);
    if (path.indexOf("/api/v1/jobs/") === 0) {
      const id = path.split("/")[4];
      const found = JOBS.jobs.filter(function (j) { return j.job_id === id; })[0];
      if (found) return json({ summary: found, events: [], request: {}, result: {} });
      return json({ error: { code: "job_not_found", message: "unknown job" } }, 404);
    }
    if (path === "/api/v1/faucet") return json(FAUCET);
    return json({}, 404);
  };
})();
