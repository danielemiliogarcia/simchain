(function (root, factory) {
  "use strict";
  const api = Object.freeze(factory());
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.SimchainFeeUnits = api;
}(typeof globalThis === "object" ? globalThis : this, function () {
  "use strict";

  const BTC_KVB = "btc-kvb";
  const SAT_VB = "sat-vb";
  const SAT_VB_PER_BTC_KVB = 100000;
  const SUPPORTED_UNITS = new Set([BTC_KVB, SAT_VB]);

  function failure(error) {
    return { ok: false, error };
  }

  function decimalString(value) {
    if (!Number.isFinite(value)) return null;
    const fixed = value.toFixed(12).replace(/\.?0+$/, "");
    return fixed === "-0" ? "0" : fixed;
  }

  function positiveNumber(value) {
    if (typeof value === "string" && value.trim() === "") return failure("fee rate is required");
    const number = Number(value);
    if (!Number.isFinite(number) || number <= 0) {
      return failure("fee rate must be a finite positive number");
    }
    return { ok: true, value: number };
  }

  function convert(value, fromUnit, toUnit) {
    if (!SUPPORTED_UNITS.has(fromUnit) || !SUPPORTED_UNITS.has(toUnit)) {
      return failure("unsupported fee unit");
    }
    const parsed = positiveNumber(value);
    if (!parsed.ok) return parsed;
    let converted = parsed.value;
    if (fromUnit === BTC_KVB && toUnit === SAT_VB) converted *= SAT_VB_PER_BTC_KVB;
    if (fromUnit === SAT_VB && toUnit === BTC_KVB) converted /= SAT_VB_PER_BTC_KVB;
    const formatted = decimalString(converted);
    return formatted == null ? failure("fee rate conversion failed") : { ok: true, value: formatted };
  }

  function displayLiveCanonical(canonical, unit) {
    return convert(canonical, BTC_KVB, unit);
  }

  function editLive(state, displayValue, unit) {
    const converted = convert(displayValue, unit, BTC_KVB);
    if (!converted.ok) return { ...converted, state };
    return {
      ok: true,
      state: {
        ...state,
        dirty: converted.value === state.desired ? null : converted.value,
      },
    };
  }

  function switchLiveUnit(state, unit) {
    if (!SUPPORTED_UNITS.has(unit)) return { ...failure("unsupported fee unit"), state };
    return { ok: true, state: { ...state, unit } };
  }

  function pollLive(state, desired, effective) {
    const desiredResult = convert(desired, BTC_KVB, BTC_KVB);
    const effectiveResult = convert(effective, BTC_KVB, BTC_KVB);
    if (!desiredResult.ok || !effectiveResult.ok) {
      return { ...failure("invalid canonical fee state"), state };
    }
    return {
      ok: true,
      state: { ...state, desired: desiredResult.value, effective: effectiveResult.value },
    };
  }

  function resetLive(state) {
    return { ok: true, state: { ...state, dirty: null } };
  }

  function liveView(state) {
    const edited = state.dirty == null ? state.desired : state.dirty;
    const input = displayLiveCanonical(edited, state.unit);
    const effective = displayLiveCanonical(state.effective, state.unit);
    if (!input.ok || !effective.ok) return failure("invalid canonical fee state");
    return {
      ok: true,
      value: { input: input.value, effective: effective.value, dirty: state.dirty != null },
    };
  }

  function livePatch(state) {
    return state.dirty == null
      ? { ok: true, value: {} }
      : { ok: true, value: { SPAM_FEE: state.dirty } };
  }

  function switchBurstUnit(canonicalSatVb, unit) {
    return convert(canonicalSatVb, SAT_VB, unit);
  }

  function burstRequest(action, base, override) {
    if (action !== "spam-prepare" && action !== "spam-burst") {
      return failure("unsupported burst action");
    }
    const request = { ...base };
    if (override && override.enabled) {
      const converted = convert(override.value, override.unit, SAT_VB);
      if (!converted.ok) return converted;
      request.fee_rate_sat_vb = Number(converted.value);
    }
    return { ok: true, value: { path: `/api/v1/jobs/${action}`, request } };
  }

  async function submitBurst(action, base, override, sender) {
    const prepared = burstRequest(action, base, override);
    if (!prepared.ok) return prepared;
    return { ok: true, value: await sender(prepared.value.path, prepared.value.request) };
  }

  return {
    BTC_KVB,
    SAT_VB,
    convert,
    displayLiveCanonical,
    editLive,
    switchLiveUnit,
    pollLive,
    resetLive,
    liveView,
    livePatch,
    switchBurstUnit,
    burstRequest,
    submitBurst,
  };
}));
