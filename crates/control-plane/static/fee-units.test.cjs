"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fees = require("./fee-units.js");

function liveState() {
  return { desired: "0.1", effective: "0.1", dirty: null, unit: fees.BTC_KVB };
}

test("live defaults display canonically and a unit switch does not dirty state", () => {
  const initial = fees.liveView(liveState());
  assert.deepEqual(initial, {
    ok: true,
    value: { input: "0.1", effective: "0.1", dirty: false },
  });

  const switched = fees.switchLiveUnit(liveState(), fees.SAT_VB);
  assert.equal(switched.ok, true);
  assert.deepEqual(fees.liveView(switched.state), {
    ok: true,
    value: { input: "10000", effective: "10000", dirty: false },
  });
});

test("live sat/vB edits produce only the canonical SPAM_FEE patch field", () => {
  const switched = fees.switchLiveUnit(liveState(), fees.SAT_VB).state;
  const edited = fees.editLive(switched, "2500", fees.SAT_VB);
  assert.equal(edited.ok, true);
  assert.equal(edited.state.dirty, "0.025");
  assert.deepEqual(fees.livePatch(edited.state), {
    ok: true,
    value: { SPAM_FEE: "0.025" },
  });
});

test("live polling preserves a dirty edit and reset adopts the new desired value", () => {
  let state = fees.switchLiveUnit(liveState(), fees.SAT_VB).state;
  state = fees.editLive(state, "2500", fees.SAT_VB).state;
  state = fees.pollLive(state, "0.2", "0.15").state;
  assert.deepEqual(fees.liveView(state).value, {
    input: "2500",
    effective: "15000",
    dirty: true,
  });

  state = fees.resetLive(state).state;
  assert.deepEqual(fees.liveView(state).value, {
    input: "20000",
    effective: "15000",
    dirty: false,
  });
});

test("invalid live inputs fail closed without changing state or calling a sender", async () => {
  const invalid = ["", "text", "NaN", "Infinity", "-Infinity", "0", "-1"];
  for (const value of invalid) {
    const state = liveState();
    const result = fees.editLive(state, value, fees.SAT_VB);
    assert.equal(result.ok, false, value);
    assert.strictEqual(result.state, state, value);
  }
  assert.equal(fees.editLive(liveState(), "10", "unknown").ok, false);

  let calls = 0;
  const result = await fees.submitBurst(
    "spam-burst",
    { node: "node2", txs: 1, data_bytes: 100 },
    { enabled: true, value: "NaN", unit: fees.SAT_VB },
    async () => { calls += 1; },
  );
  assert.equal(result.ok, false);
  assert.equal(calls, 0);
});

test("burst default remains 10 sat/vB for both actions and disabled override is omitted", () => {
  const base = { node: "node2", txs: 10, data_bytes: 20000 };
  for (const action of ["spam-prepare", "spam-burst"]) {
    const enabled = fees.burstRequest(action, base, {
      enabled: true,
      value: "10",
      unit: fees.SAT_VB,
    });
    assert.equal(enabled.ok, true);
    assert.equal(enabled.value.request.fee_rate_sat_vb, 10);

    const disabled = fees.burstRequest(action, base, { enabled: false });
    assert.equal(disabled.ok, true);
    assert.equal(Object.hasOwn(disabled.value.request, "fee_rate_sat_vb"), false);
  }
});

test("BTC/kvB burst conversion preserves paths and data/output request shapes", async () => {
  const cases = [
    ["spam-prepare", { node: "node2", txs: 2, data_bytes: 40 }],
    ["spam-burst", { node: "node3", txs: 3, outputs_per_tx: 5 }],
  ];
  for (const [action, base] of cases) {
    const calls = [];
    const result = await fees.submitBurst(
      action,
      base,
      { enabled: true, value: "0.0001", unit: fees.BTC_KVB },
      async (path, request) => {
        calls.push({ path, request });
        return "sent";
      },
    );
    assert.deepEqual(result, { ok: true, value: "sent" });
    assert.deepEqual(calls, [{
      path: `/api/v1/jobs/${action}`,
      request: { ...base, fee_rate_sat_vb: 10 },
    }]);
  }
});

test("burst display switches round-trip from canonical sat/vB without drift", () => {
  assert.deepEqual(fees.switchBurstUnit("10", fees.BTC_KVB), { ok: true, value: "0.0001" });
  assert.deepEqual(fees.switchBurstUnit("10", fees.SAT_VB), { ok: true, value: "10" });
});

test("invalid burst overrides never call the sender for either action", async () => {
  const invalid = ["", "text", "NaN", "Infinity", "-Infinity", "0", "-1"];
  for (const action of ["spam-prepare", "spam-burst"]) {
    for (const value of invalid) {
      let calls = 0;
      const result = await fees.submitBurst(
        action,
        { node: "node2", txs: 1, data_bytes: 10 },
        { enabled: true, value, unit: fees.SAT_VB },
        async () => { calls += 1; },
      );
      assert.equal(result.ok, false, `${action}: ${value}`);
      assert.equal(calls, 0, `${action}: ${value}`);
    }
    let calls = 0;
    const unknown = await fees.submitBurst(
      action,
      { node: "node2", txs: 1, data_bytes: 10 },
      { enabled: true, value: "10", unit: "unknown" },
      async () => { calls += 1; },
    );
    assert.equal(unknown.ok, false);
    assert.equal(calls, 0);
  }
});
