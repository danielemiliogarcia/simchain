//! Self-describing catalog of the declarative scenario language.
//!
//! This is the scenario counterpart of `live_tuning::MANAGED_SETTINGS`: one
//! static entry per step type and per nested object, carrying the field names,
//! value types, requirement rules, defaults, and constraints that `validate`
//! enforces. The control plane serves it, the CLI prints it, and the reference
//! section of `docs/SCENARIOS.md` is written from it, so the language has a
//! single description instead of one per consumer.
//!
//! The tests below pin the catalog to `Step` itself: every step kind must be
//! described, and every described field must match the field names serde
//! actually accepts. A new step or a renamed field fails the suite rather than
//! silently leaving the documentation behind.

use crate::schema::BOOTSTRAP_HEIGHT;
use simchain_common::control_api::{
    ScenarioFieldSchema, ScenarioObjectSchema, ScenarioSchemaResponse, ScenarioStepSchema,
    ScenarioVariantSchema,
};

/// How a field is spelled in YAML.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
    Integer,
    Float,
    Boolean,
    String,
    /// Decimal BTC (`1`, `0.25`, `1btc`) or integer satoshis (`25000000sat`).
    Amount,
    /// A fixed set of accepted values.
    Choice(&'static [&'static str]),
    /// Free-form `KEY: value` mapping.
    StringMap,
    /// Inline object described by the named entry in [`OBJECT_CATALOG`].
    Object(&'static str),
    /// List of objects described by the named entry in [`OBJECT_CATALOG`].
    ObjectList(&'static str),
}

impl FieldType {
    /// Stable machine-readable name, used by the served schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Boolean => "boolean",
            Self::String => "string",
            Self::Amount => "amount",
            Self::Choice(_) => "choice",
            Self::StringMap => "string_map",
            Self::Object(_) => "object",
            Self::ObjectList(_) => "object_list",
        }
    }

    /// Accepted values for a `choice` field.
    pub fn options(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Choice(options) => Some(options),
            _ => None,
        }
    }

    /// Name of the [`OBJECT_CATALOG`] entry describing this field's shape.
    pub fn object(self) -> Option<&'static str> {
        match self {
            Self::Object(name) | Self::ObjectList(name) => Some(name),
            _ => None,
        }
    }
}

/// Whether a field must be present, and which cross-field rule governs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Requirement {
    Required,
    Optional,
    /// Exactly one field in the named group must be set.
    ExactlyOneOf(&'static str),
    /// At least one field in the named group must be set.
    AtLeastOneOf(&'static str),
    /// Required only when the described condition holds.
    RequiredWhen(&'static str),
}

impl Requirement {
    /// Stable machine-readable name, used by the served schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::ExactlyOneOf(_) => "exactly_one_of",
            Self::AtLeastOneOf(_) => "at_least_one_of",
            Self::RequiredWhen(_) => "required_when",
        }
    }

    /// Group label for `exactly_one_of` / `at_least_one_of`, or the condition
    /// text for `required_when`.
    pub fn group(self) -> Option<&'static str> {
        match self {
            Self::ExactlyOneOf(group) | Self::AtLeastOneOf(group) => Some(group),
            Self::RequiredWhen(condition) => Some(condition),
            Self::Required | Self::Optional => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    pub name: &'static str,
    pub value_type: FieldType,
    pub requirement: Requirement,
    /// Value applied when the field is omitted, if any.
    pub default: Option<&'static str>,
    /// Validation bounds and behavior worth knowing before writing the field.
    pub help: &'static str,
}

/// What a step needs beyond the control plane itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepRequires {
    /// Runs under any profile that has a control plane (`minimal-api` and up).
    ControlPlane,
    /// Needs the namespace-local network agents (`minimal-organic-reorg` and up).
    NetworkAgents,
}

impl StepRequires {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ControlPlane => "control-plane",
            Self::NetworkAgents => "network-agents",
        }
    }

    /// Smallest Compose profile that can run the step.
    pub fn minimum_profile(self) -> &'static str {
        match self {
            Self::ControlPlane => "minimal-api",
            Self::NetworkAgents => "minimal-organic-reorg",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StepSpec {
    /// Value of the step's `type` key.
    pub kind: &'static str,
    /// One-line description of what the step does.
    pub summary: &'static str,
    pub requires: StepRequires,
    pub fields: &'static [FieldSpec],
    /// Behavior that is not a per-field rule.
    pub notes: &'static [&'static str],
}

/// A nested object reachable from a step field.
#[derive(Clone, Copy, Debug)]
pub struct ObjectSpec {
    pub name: &'static str,
    pub summary: &'static str,
    /// Discriminator key for a tagged union, e.g. `kind` for `wait_condition`.
    pub tag: Option<&'static str>,
    /// Variants of a tagged union; empty for plain objects.
    pub variants: &'static [VariantSpec],
    /// Fields present on every variant, or all fields of a plain object.
    pub fields: &'static [FieldSpec],
}

#[derive(Clone, Copy, Debug)]
pub struct VariantSpec {
    /// Value of the discriminator key.
    pub tag_value: &'static str,
    pub summary: &'static str,
    pub fields: &'static [FieldSpec],
}

const MINER_NODES: &[&str] = &["btc-simnet-node2", "btc-simnet-node3"];
const NETWORK_NODES: &[&str] = &["node1", "node2", "node3"];
const COMPONENTS: &[&str] = &[
    "mining",
    "spam",
    "network-agent-node1",
    "network-agent-node2",
    "network-agent-node3",
];
const DESIRED_STATES: &[&str] = &["running", "paused"];
const TX_WAIT_STATES: &[&str] = &["seen", "mempool", "confirmed", "missing"];
const FAUCET_SOURCES: &[&str] = &["auto", "node2", "node3"];

/// Fields every component expectation shares, used by `assert_component` and
/// by the `component` wait condition. At least one expectation beyond
/// `component` itself must be set.
const COMPONENT_EXPECTATION_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "component",
        value_type: FieldType::Choice(COMPONENTS),
        requirement: Requirement::Required,
        default: None,
        help: "Which component the expectation reads.",
    },
    FieldSpec {
        name: "reachable",
        value_type: FieldType::Boolean,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Whether the control plane can reach the component's internal API.",
    },
    FieldSpec {
        name: "status",
        value_type: FieldType::String,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Reported component status string, for example `active`.",
    },
    FieldSpec {
        name: "phase",
        value_type: FieldType::String,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Reported worker phase string.",
    },
    FieldSpec {
        name: "desired_state",
        value_type: FieldType::Choice(DESIRED_STATES),
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Durable desired state recorded by the control plane.",
    },
    FieldSpec {
        name: "effective_state",
        value_type: FieldType::Choice(DESIRED_STATES),
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "State the worker currently exposes, which lags desired state across a safe point.",
    },
    FieldSpec {
        name: "effective_generation",
        value_type: FieldType::Integer,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Desired-state generation the worker has applied.",
    },
    FieldSpec {
        name: "observed_height_at_least",
        value_type: FieldType::Integer,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Minimum chain height the component reports having observed.",
    },
    FieldSpec {
        name: "active_lease_count",
        value_type: FieldType::Integer,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Number of job-owned leases currently held against the component.",
    },
    FieldSpec {
        name: "cycle_phase",
        value_type: FieldType::String,
        requirement: Requirement::AtLeastOneOf("expectation"),
        default: None,
        help: "Reported position within the component's work cycle.",
    },
];

/// Nested object shapes reachable from step fields.
pub const OBJECT_CATALOG: &[ObjectSpec] = &[
    ObjectSpec {
        name: "wait_condition",
        summary: "Predicate polled by `wait_until` until it holds or the timeout expires.",
        tag: Some("kind"),
        variants: &[
            VariantSpec {
                tag_value: "height_at_least",
                summary: "Waits until node1 reaches an absolute height.",
                fields: &[FieldSpec {
                    name: "height",
                    value_type: FieldType::Integer,
                    requirement: Requirement::Required,
                    default: None,
                    help: "Target height; at least 204.",
                }],
            },
            VariantSpec {
                tag_value: "mempool_txs_at_least",
                summary: "Waits until the mempool holds at least `count` transactions.",
                fields: &[FieldSpec {
                    name: "count",
                    value_type: FieldType::Integer,
                    requirement: Requirement::Required,
                    default: None,
                    help: "Minimum mempool transaction count.",
                }],
            },
            VariantSpec {
                tag_value: "mempool_txs_at_most",
                summary: "Waits until the mempool holds at most `count` transactions.",
                fields: &[FieldSpec {
                    name: "count",
                    value_type: FieldType::Integer,
                    requirement: Requirement::Required,
                    default: None,
                    help: "Maximum mempool transaction count.",
                }],
            },
            VariantSpec {
                tag_value: "component",
                summary: "Waits until a component matches the given expectations.",
                fields: COMPONENT_EXPECTATION_FIELDS,
            },
        ],
        fields: &[],
    },
    ObjectSpec {
        name: "faucet_output",
        summary: "One destination in a `faucet` transfer.",
        tag: None,
        variants: &[],
        fields: &[
            FieldSpec {
                name: "address",
                value_type: FieldType::String,
                requirement: Requirement::ExactlyOneOf("destination"),
                default: None,
                help: "Literal destination address.",
            },
            FieldSpec {
                name: "address_env",
                value_type: FieldType::String,
                requirement: Requirement::ExactlyOneOf("destination"),
                default: None,
                help: "Environment variable holding the address. Alphanumerics and `_` only. \
                       `simchainctl` and the standalone submitter resolve it in the client \
                       process before upload; raw API submissions resolve it in the control plane.",
            },
            FieldSpec {
                name: "amount",
                value_type: FieldType::Amount,
                requirement: Requirement::Required,
                default: None,
                help: "Positive amount, always a string: decimal BTC (`\"1\"`, `\"0.25\"`, \
                       `1btc`) or integer satoshis with a `sat` suffix (`25000000sat`). A \
                       suffixed value is already a YAML string, but a bare number must be \
                       quoted or YAML hands the parser an integer or float and the file is \
                       rejected.",
            },
        ],
    },
];

/// Every step type, in the order the reference documentation presents them.
pub const STEP_CATALOG: &[StepSpec] = &[
    StepSpec {
        kind: "wait_height",
        summary: "Waits until node1 reaches an absolute chain height.",
        requires: StepRequires::ControlPlane,
        fields: &[FieldSpec {
            name: "height",
            value_type: FieldType::Integer,
            requirement: Requirement::Required,
            default: None,
            help: "Target height; at least 204. Returns immediately when the chain is already \
                   past it.",
        }],
        notes: &[
            "Absolute. Prefer `wait_n_blocks` unless the test genuinely needs a fixed height, \
             because an absolute target behaves differently on a fresh chain than on one that \
             has been running for hours.",
        ],
    },
    StepSpec {
        kind: "wait_n_blocks",
        summary: "Waits for `n` more blocks than node1 has when the step starts.",
        requires: StepRequires::ControlPlane,
        fields: &[FieldSpec {
            name: "n",
            value_type: FieldType::Integer,
            requirement: Requirement::Required,
            default: None,
            help: "Positive number of additional blocks.",
        }],
        notes: &[
            "Relative, so the same file behaves the same way regardless of the chain's current \
             height. This is the right default for \"N more blocks from here\".",
        ],
    },
    StepSpec {
        kind: "wait_until",
        summary: "Polls a condition until it holds or the timeout expires.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "condition",
                value_type: FieldType::Object("wait_condition"),
                requirement: Requirement::Required,
                default: None,
                help: "Predicate to poll, tagged by `kind`.",
            },
            FieldSpec {
                name: "timeout_secs",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("900"),
                help: "Positive. Failing the timeout fails the step and the job.",
            },
        ],
        notes: &[],
    },
    StepSpec {
        kind: "wait_tx",
        summary: "Waits for one caller-supplied transaction to reach a target state.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "txid",
                value_type: FieldType::String,
                requirement: Requirement::ExactlyOneOf("transaction"),
                default: None,
                help: "64 hexadecimal characters. Quote it in YAML, because an all-digit hex \
                       string is otherwise parsed as a number.",
            },
            FieldSpec {
                name: "txid_env",
                value_type: FieldType::String,
                requirement: Requirement::ExactlyOneOf("transaction"),
                default: None,
                help: "Environment variable holding the txid. Alphanumerics and `_` only.",
            },
            FieldSpec {
                name: "state",
                value_type: FieldType::Choice(TX_WAIT_STATES),
                requirement: Requirement::Optional,
                default: Some("confirmed"),
                help: "Target state to wait for.",
            },
            FieldSpec {
                name: "confirmations",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("1"),
                help: "Positive, and only valid with `state: confirmed`.",
            },
            FieldSpec {
                name: "timeout_secs",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("900"),
                help: "Positive.",
            },
        ],
        notes: &[
            "Lets the scenario itself decide when to continue from a transaction the application \
             under test broadcast, without indexing or tagging every transaction. Use a \
             `checkpoint` instead when an external caller should make that decision.",
            "Combines with `reorg` to test orphaning: wait for `confirmed` with two \
             confirmations, run an empty reorg deep enough to orphan it, then wait for \
             `state: mempool`.",
        ],
    },
    StepSpec {
        kind: "assert_height",
        summary: "Asserts node1's current height without waiting.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "equals",
                value_type: FieldType::Integer,
                requirement: Requirement::AtLeastOneOf("condition"),
                default: None,
                help: "Exact height. Cannot be combined with `at_least` or `at_most`.",
            },
            FieldSpec {
                name: "at_least",
                value_type: FieldType::Integer,
                requirement: Requirement::AtLeastOneOf("condition"),
                default: None,
                help: "Inclusive lower bound; must not exceed `at_most`.",
            },
            FieldSpec {
                name: "at_most",
                value_type: FieldType::Integer,
                requirement: Requirement::AtLeastOneOf("condition"),
                default: None,
                help: "Inclusive upper bound.",
            },
        ],
        notes: &[],
    },
    StepSpec {
        kind: "assert_component",
        summary: "Asserts a component's reported state without waiting.",
        requires: StepRequires::ControlPlane,
        fields: COMPONENT_EXPECTATION_FIELDS,
        notes: &[
            "At least one expectation beyond `component` must be set, otherwise the step asserts \
             nothing and the file is rejected.",
        ],
    },
    StepSpec {
        kind: "sleep",
        summary: "Waits a fixed wall-clock duration.",
        requires: StepRequires::ControlPlane,
        fields: &[FieldSpec {
            name: "secs",
            value_type: FieldType::Integer,
            requirement: Requirement::Required,
            default: None,
            help: "Positive seconds.",
        }],
        notes: &[
            "Prefer `wait_until` or `wait_n_blocks` where a real condition exists; a sleep that \
             is long enough on one machine can be too short on another.",
        ],
    },
    StepSpec {
        kind: "pause_mining",
        summary: "Takes a job-owned mining lease and holds block production paused.",
        requires: StepRequires::ControlPlane,
        fields: &[],
        notes: &[
            "The lease is released by a later `resume_mining` step or by cleanup when the job \
             ends, so a failed scenario never leaves mining paused.",
        ],
    },
    StepSpec {
        kind: "resume_mining",
        summary: "Releases the mining lease taken by `pause_mining`.",
        requires: StepRequires::ControlPlane,
        fields: &[],
        notes: &[],
    },
    StepSpec {
        kind: "mine",
        summary: "Mines a fixed number of blocks on one miner node.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "node",
                value_type: FieldType::Choice(MINER_NODES),
                requirement: Requirement::Required,
                default: None,
                help: "Miner node. `node2` and `node3` are accepted aliases. node1 refuses \
                       mining RPCs and cannot be used.",
            },
            FieldSpec {
                name: "blocks",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Positive block count.",
            },
        ],
        notes: &[
            "Pair with `pause_mining` when the test needs the manual blocks to be the only \
                  ones produced.",
        ],
    },
    StepSpec {
        kind: "reorg",
        summary: "Creates a deterministic chain reorganization.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "depth",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Blocks to orphan; 1 through 100.",
            },
            FieldSpec {
                name: "empty",
                value_type: FieldType::Boolean,
                requirement: Requirement::Optional,
                default: Some("false"),
                help: "Replacement blocks carry no transactions beyond the coinbase.",
            },
            FieldSpec {
                name: "node",
                value_type: FieldType::Choice(MINER_NODES),
                requirement: Requirement::Optional,
                default: Some("btc-simnet-node3"),
                help: "Node that builds the replacement branch.",
            },
            FieldSpec {
                name: "adds_new_txs",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("0"),
                help: "At most 10000. Prioritizes fresh wallet transactions into the \
                       replacement blocks.",
            },
            FieldSpec {
                name: "double_spend_pct",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("0"),
                help: "0 through 100. Exposes the permanent-drop conflict path, where orphaned \
                       transactions do not return to the mempool.",
            },
        ],
        notes: &[
            "Takes both mining and spam leases, and witnesses strict node1 convergence before \
             the step completes. Fields match `simchainctl reorg start`.",
        ],
    },
    StepSpec {
        kind: "spam_burst",
        summary: "Broadcasts a burst of raw transactions from a dedicated engine.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "node",
                value_type: FieldType::Choice(MINER_NODES),
                requirement: Requirement::Required,
                default: None,
                help: "Node whose wallet funds the burst engine.",
            },
            FieldSpec {
                name: "txs",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Positive transaction count. Also sets how many confirmed branches the \
                       job funds up front, since a burst reserves one branch per transaction.",
            },
            FieldSpec {
                name: "outputs_per_tx",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "May be zero. Zero sends sequential single-output transactions; a positive \
                       value sends that many 546-sat burn outputs per transaction.",
            },
        ],
        notes: &[
            "Bursts run on a dedicated raw engine — locally signed, submitted with \
             `sendrawtransaction`, priced from the live `SPAM_FEE` — so no coin-selection or \
             signing load lands on the miner node wallets.",
            "The job funds every burst engine before step 1 runs, while mining still produces \
             blocks, because funding needs confirmations and scenarios often pause mining before \
             their first burst. A `set_config` step that changes spam policy refunds the bursts \
             still ahead of it.",
        ],
    },
    StepSpec {
        kind: "set_config",
        summary: "Applies a partial runtime desired-state patch.",
        requires: StepRequires::ControlPlane,
        fields: &[FieldSpec {
            name: "settings",
            value_type: FieldType::StringMap,
            requirement: Requirement::Required,
            default: None,
            help: "Non-empty map using the same keys as `simchainctl config set`. Values may be \
                   strings, numbers, booleans, or null/empty reset values. Keys must not be blank.",
        }],
        notes: &[
            "Uses the same validation, worker apply, verification, persistence, and rollback path \
             as the dashboard and CLI.",
            "With top-level `restore_settings: true`, the complete pre-scenario desired map is \
             durably captured before execution and restored after success, failure, abort, panic, \
             or control-plane restart. Config mutation stays blocked until restoration completes.",
        ],
    },
    StepSpec {
        kind: "assert_config",
        summary: "Asserts runtime configuration values.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "settings",
                value_type: FieldType::StringMap,
                requirement: Requirement::Required,
                default: None,
                help: "Non-empty map of expected values.",
            },
            FieldSpec {
                name: "effective",
                value_type: FieldType::Boolean,
                requirement: Requirement::Optional,
                default: Some("true"),
                help: "Also require that the mining and spam workers expose the expected \
                       effective policy at the current desired generation, not just that the \
                       durable desired values match.",
            },
        ],
        notes: &[
            "`effective: false` checks only durable desired values, which is the right choice \
             immediately after a `set_config` whose apply mode defers to the next safe point.",
        ],
    },
    StepSpec {
        kind: "faucet",
        summary: "Funds addresses from a miner node wallet.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "outputs",
                value_type: FieldType::ObjectList("faucet_output"),
                requirement: Requirement::Required,
                default: None,
                help: "1 through 100 destinations.",
            },
            FieldSpec {
                name: "source",
                value_type: FieldType::Choice(FAUCET_SOURCES),
                requirement: Requirement::Optional,
                default: Some("auto"),
                help: "Funding wallet. `auto` picks a miner node with sufficient balance.",
            },
            FieldSpec {
                name: "wait_confirmed",
                value_type: FieldType::Boolean,
                requirement: Requirement::Optional,
                default: Some("true"),
                help: "Wait until the transfer confirms before continuing.",
            },
            FieldSpec {
                name: "timeout_secs",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("900"),
                help: "Positive.",
            },
        ],
        notes: &[],
    },
    StepSpec {
        kind: "partition",
        summary: "Splits one node off the network, builds competing branches, then heals.",
        requires: StepRequires::NetworkAgents,
        fields: &[
            FieldSpec {
                name: "node",
                value_type: FieldType::Choice(MINER_NODES),
                requirement: Requirement::Required,
                default: None,
                help: "Node to isolate.",
            },
            FieldSpec {
                name: "main_blocks",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Positive. Blocks mined on the majority side during the split.",
            },
            FieldSpec {
                name: "isolated_blocks",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Positive, and must differ from `main_blocks` so the winning branch is \
                       deterministic.",
            },
            FieldSpec {
                name: "heal_delay_secs",
                value_type: FieldType::Integer,
                requirement: Requirement::Optional,
                default: Some("0"),
                help: "At most 86400. Holds the completed competing branches apart before \
                       healing, which is where an application can observe the split.",
            },
        ],
        notes: &[
            "Leases the target's namespace-local network agent, blocks P2P ingress and egress, \
             mines both branches, heals, and witnesses the deterministic winner before worker \
             leases can resume.",
        ],
    },
    StepSpec {
        kind: "degrade",
        summary: "Applies bounded network impairment to one node, then releases it.",
        requires: StepRequires::NetworkAgents,
        fields: &[
            FieldSpec {
                name: "node",
                value_type: FieldType::Choice(NETWORK_NODES),
                requirement: Requirement::Required,
                default: None,
                help: "Target node. `btc-simnet-*` names are accepted aliases. Unlike mining \
                       steps, node1 is allowed here.",
            },
            FieldSpec {
                name: "delay_ms",
                value_type: FieldType::Integer,
                requirement: Requirement::Required,
                default: None,
                help: "Added latency, at most 600000. Always required, even for a pure packet \
                       loss impairment: write `delay_ms: 0` and set `loss_pct`.",
            },
            FieldSpec {
                name: "loss_pct",
                value_type: FieldType::Float,
                requirement: Requirement::Optional,
                default: Some("0"),
                help: "Packet loss percentage; finite, 0 through 100.",
            },
            FieldSpec {
                name: "seconds",
                value_type: FieldType::Integer,
                requirement: Requirement::ExactlyOneOf("duration"),
                default: None,
                help: "1 through 86400.",
            },
            FieldSpec {
                name: "until_height",
                value_type: FieldType::Integer,
                requirement: Requirement::ExactlyOneOf("duration"),
                default: None,
                help: "At least 204. Holds the impairment until node1 reaches this height.",
            },
        ],
        notes: &[
            "At least one of `delay_ms` or `loss_pct` must be positive; a step that impairs \
             nothing is rejected. That is a value rule, not a presence rule — `delay_ms` must \
             still appear.",
            "Leases the target network agent, applies bounded `netem`, then releases it.",
        ],
    },
    StepSpec {
        kind: "checkpoint",
        summary: "Records a durable milestone, and by default pauses until released.",
        requires: StepRequires::ControlPlane,
        fields: &[
            FieldSpec {
                name: "name",
                value_type: FieldType::String,
                requirement: Requirement::Required,
                default: None,
                help: "Non-empty, URL-safe (alphanumerics and `-`, `_`, `.`, `~`), at most 100 \
                       bytes, and unique within the file.",
            },
            FieldSpec {
                name: "pause",
                value_type: FieldType::Boolean,
                requirement: Requirement::Optional,
                default: Some("true"),
                help: "`false` records the milestone and continues immediately.",
            },
            FieldSpec {
                name: "timeout_secs",
                value_type: FieldType::Integer,
                requirement: Requirement::RequiredWhen("pause is true"),
                default: None,
                help: "Positive. Expiring fails the job and triggers cleanup.",
            },
        ],
        notes: &[
            "On arrival the server durably records a unique generation and a full live \
             chain/mining/spam summary before exposing the reached state.",
            "Use a checkpoint when an external harness or a human should decide when the \
             scenario continues; use `wait_tx` when the scenario itself can decide from a txid.",
            "Release is idempotent for the same generation, and stale generations are rejected \
             with a conflict.",
        ],
    },
];

/// Look up one step's description.
pub fn step_spec(kind: &str) -> Option<&'static StepSpec> {
    STEP_CATALOG.iter().find(|spec| spec.kind == kind)
}

/// Look up one nested object's description.
pub fn object_spec(name: &str) -> Option<&'static ObjectSpec> {
    OBJECT_CATALOG.iter().find(|spec| spec.name == name)
}

/// Bootstrap height every scenario waits for before step 1, surfaced so the
/// served schema does not restate a constant the engine already owns.
pub const CATALOG_BOOTSTRAP_HEIGHT: u64 = BOOTSTRAP_HEIGHT;

/// Only accepted value of a scenario file's `version` key. Unrelated to the
/// control plane's persisted job-store `schema_version`.
pub const SCENARIO_VERSION: u64 = 1;

/// Render the catalog as the public transport shape. The HTTP endpoint, the
/// MCP tool, and `simchainctl scenario schema` all call this, so no consumer
/// can describe the language differently from another.
pub fn schema_response() -> ScenarioSchemaResponse {
    ScenarioSchemaResponse {
        version: SCENARIO_VERSION,
        bootstrap_height: CATALOG_BOOTSTRAP_HEIGHT,
        steps: STEP_CATALOG
            .iter()
            .map(|spec| ScenarioStepSchema {
                kind: spec.kind.to_string(),
                summary: spec.summary.to_string(),
                requires: spec.requires.as_str().to_string(),
                minimum_profile: spec.requires.minimum_profile().to_string(),
                fields: spec.fields.iter().map(field_schema).collect(),
                notes: spec.notes.iter().map(|note| (*note).to_string()).collect(),
            })
            .collect(),
        objects: OBJECT_CATALOG
            .iter()
            .map(|object| ScenarioObjectSchema {
                name: object.name.to_string(),
                summary: object.summary.to_string(),
                tag: object.tag.map(str::to_string),
                variants: object
                    .variants
                    .iter()
                    .map(|variant| ScenarioVariantSchema {
                        tag_value: variant.tag_value.to_string(),
                        summary: variant.summary.to_string(),
                        fields: variant.fields.iter().map(field_schema).collect(),
                    })
                    .collect(),
                fields: object.fields.iter().map(field_schema).collect(),
            })
            .collect(),
    }
}

/// Markers delimiting the generated reference inside `docs/SCENARIOS.md`.
pub const REFERENCE_BEGIN: &str = "<!-- BEGIN GENERATED STEP REFERENCE -->";
pub const REFERENCE_END: &str = "<!-- END GENERATED STEP REFERENCE -->";

/// Render the catalog as the Markdown reference committed to
/// `docs/SCENARIOS.md`. `simchainctl scenario schema --markdown` prints it and
/// a test asserts the committed file still matches, so the prose cannot fall
/// behind the language.
pub fn reference_markdown() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Generated from the engine catalog by `simchainctl scenario schema --markdown`.\n\
         Every scenario file declares `version: {SCENARIO_VERSION}` and waits for node1 to reach\n\
         height {CATALOG_BOOTSTRAP_HEIGHT} before step 1.\n"
    ));

    out.push_str("\n### Steps at a glance\n\n");
    out.push_str("| Step | Purpose | Needs |\n|---|---|---|\n");
    for spec in STEP_CATALOG {
        let needs = match spec.requires {
            StepRequires::ControlPlane => "control plane".to_string(),
            StepRequires::NetworkAgents => {
                format!("network agents (`{}`)", spec.requires.minimum_profile())
            }
        };
        // GitHub keeps underscores in heading anchors, so the link target is
        // the step kind verbatim.
        out.push_str(&format!(
            "| [`{}`](#{}) | {} | {needs} |\n",
            spec.kind,
            spec.kind,
            escape_cell(spec.summary)
        ));
    }

    for spec in STEP_CATALOG {
        out.push_str(&format!("\n### `{}`\n\n{}\n", spec.kind, spec.summary));
        if spec.requires == StepRequires::NetworkAgents {
            out.push_str(&format!(
                "\nRequires the namespace-local network agents; the smallest profile that has \
                 them is `{}`. A scenario using this step is rejected whole at submission under \
                 a smaller profile.\n",
                spec.requires.minimum_profile()
            ));
        }
        if spec.fields.is_empty() {
            out.push_str("\nTakes no fields.\n");
        } else {
            out.push_str(
                "\n| Field | Type | Required | Default | Notes |\n|---|---|---|---|---|\n",
            );
            for field in spec.fields {
                out.push_str(&field_row(field));
            }
        }
        for note in spec.notes {
            out.push_str(&format!("\n{note}\n"));
        }
    }

    out.push_str("\n### Nested objects\n");
    for object in OBJECT_CATALOG {
        out.push_str(&format!("\n#### `{}`\n\n{}\n", object.name, object.summary));
        if !object.fields.is_empty() {
            out.push_str(
                "\n| Field | Type | Required | Default | Notes |\n|---|---|---|---|---|\n",
            );
            for field in object.fields {
                out.push_str(&field_row(field));
            }
        }
        for variant in object.variants {
            let tag = object.tag.unwrap_or("kind");
            out.push_str(&format!(
                "\n**`{tag}: {}`** — {}\n",
                variant.tag_value, variant.summary
            ));
            if variant.fields.is_empty() {
                continue;
            }
            out.push_str(
                "\n| Field | Type | Required | Default | Notes |\n|---|---|---|---|---|\n",
            );
            for field in variant.fields {
                out.push_str(&field_row(field));
            }
        }
    }
    out
}

fn field_row(field: &FieldSpec) -> String {
    let value_type = match field.value_type {
        FieldType::Choice(options) => options
            .iter()
            .map(|option| format!("`{option}`"))
            .collect::<Vec<_>>()
            .join(" \\| "),
        FieldType::Object(name) => format!("[`{name}`](#{name}) object"),
        FieldType::ObjectList(name) => format!("list of [`{name}`](#{name})"),
        other => format!("`{}`", other.as_str()),
    };
    let requirement = match field.requirement {
        Requirement::Required => "yes".to_string(),
        Requirement::Optional => "no".to_string(),
        Requirement::ExactlyOneOf(group) => format!("exactly one of *{group}*"),
        Requirement::AtLeastOneOf(group) => format!("at least one of *{group}*"),
        Requirement::RequiredWhen(condition) => format!("when {condition}"),
    };
    let default = field
        .default
        .map(|value| format!("`{value}`"))
        .unwrap_or_else(|| "—".to_string());
    format!(
        "| `{}` | {value_type} | {requirement} | {default} | {} |\n",
        field.name,
        escape_cell(field.help)
    )
}

/// Table cells cannot contain a bare pipe, and the catalog help text is
/// written as prose rather than Markdown-aware strings.
fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn field_schema(field: &FieldSpec) -> ScenarioFieldSchema {
    ScenarioFieldSchema {
        name: field.name.to_string(),
        value_type: field.value_type.as_str().to_string(),
        options: field
            .value_type
            .options()
            .map(|options| options.iter().map(|option| (*option).to_string()).collect()),
        object: field.value_type.object().map(str::to_string),
        requirement: field.requirement.as_str().to_string(),
        requirement_group: field.requirement.group().map(str::to_string),
        default: field.default.map(str::to_string),
        help: field.help.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        CheckpointStep, ComponentExpectation, FaucetScenarioOutput, MinerNode, NetworkNode,
        ScenarioComponent, Step, TxWaitState, WaitCondition, WaitTxStep,
    };
    use serde_json::Value;
    use simchain_common::control_api::FaucetSource;
    use simchain_common::internal_api::DesiredState;
    use std::collections::{BTreeMap, BTreeSet};

    /// Every expectation field set, so serialization emits all of them.
    fn full_expectation() -> ComponentExpectation {
        ComponentExpectation {
            component: ScenarioComponent::Mining,
            reachable: Some(true),
            status: Some("active".to_string()),
            phase: Some("running".to_string()),
            desired_state: Some(DesiredState::Running),
            effective_state: Some(DesiredState::Running),
            effective_generation: Some(1),
            observed_height_at_least: Some(204),
            active_lease_count: Some(0),
            cycle_phase: Some("idle".to_string()),
        }
    }

    /// One sample per step kind with every optional field populated, so the
    /// serialized key set is the complete set of field names serde accepts.
    fn every_step() -> Vec<Step> {
        vec![
            Step::WaitHeight { height: 204 },
            Step::WaitNBlocks { n: 1 },
            Step::WaitUntil {
                condition: WaitCondition::HeightAtLeast { height: 204 },
                timeout_secs: 900,
            },
            Step::WaitTx {
                wait: WaitTxStep {
                    txid: Some("a".repeat(64)),
                    txid_env: Some("TARGET_TXID".to_string()),
                    state: TxWaitState::Confirmed,
                    confirmations: Some(1),
                    timeout_secs: 900,
                },
            },
            Step::AssertHeight {
                equals: Some(204),
                at_least: Some(204),
                at_most: Some(204),
            },
            Step::AssertComponent {
                expected: full_expectation(),
            },
            Step::Sleep { secs: 1 },
            Step::PauseMining,
            Step::ResumeMining,
            Step::Mine {
                node: MinerNode::Node2,
                blocks: 1,
            },
            Step::Reorg {
                depth: 1,
                empty: false,
                node: MinerNode::Node3,
                adds_new_txs: 0,
                double_spend_pct: 0,
            },
            Step::SpamBurst {
                node: MinerNode::Node2,
                txs: 1,
                outputs_per_tx: 0,
            },
            Step::SetConfig {
                settings: BTreeMap::from([("SPAM_FEE".to_string(), "0.002".to_string())]),
            },
            Step::AssertConfig {
                settings: BTreeMap::from([("SPAM_FEE".to_string(), "0.002".to_string())]),
                effective: true,
            },
            Step::Faucet {
                source: FaucetSource::Auto,
                outputs: vec![FaucetScenarioOutput {
                    address: Some("bcrt1qexample".to_string()),
                    address_env: Some("FUND_ADD_1".to_string()),
                    amount: "1btc".to_string(),
                }],
                wait_confirmed: true,
                timeout_secs: 900,
            },
            Step::Partition {
                node: MinerNode::Node3,
                main_blocks: 3,
                isolated_blocks: 5,
                heal_delay_secs: 0,
            },
            Step::Degrade {
                node: NetworkNode::Node2,
                delay_ms: 500,
                loss_pct: 1.0,
                seconds: Some(60),
                until_height: Some(260),
            },
            Step::Checkpoint {
                checkpoint: CheckpointStep {
                    name: "held".to_string(),
                    pause: true,
                    timeout_secs: Some(600),
                },
            },
        ]
    }

    fn serialized_keys(value: &Value) -> BTreeSet<String> {
        value
            .as_object()
            .expect("step serializes to an object")
            .keys()
            .filter(|key| *key != "type")
            .cloned()
            .collect()
    }

    #[test]
    fn catalog_covers_every_step_kind_exactly_once() {
        let described: BTreeSet<&str> = STEP_CATALOG.iter().map(|spec| spec.kind).collect();
        assert_eq!(
            described.len(),
            STEP_CATALOG.len(),
            "a step kind is described twice"
        );
        let actual: BTreeSet<&str> = every_step().iter().map(Step::kind).collect();
        assert_eq!(
            described, actual,
            "the catalog and the Step enum describe different step kinds"
        );
    }

    #[test]
    fn catalog_field_names_match_what_serde_accepts() {
        for step in every_step() {
            let kind = step.kind();
            let spec =
                step_spec(kind).unwrap_or_else(|| panic!("{kind} is missing a catalog spec"));
            let described: BTreeSet<String> = spec
                .fields
                .iter()
                .map(|field| field.name.to_string())
                .collect();
            let serialized =
                serde_json::to_value(&step).unwrap_or_else(|_| panic!("{kind} serializes"));
            assert_eq!(
                described,
                serialized_keys(&serialized),
                "catalog fields for `{kind}` do not match the fields serde accepts"
            );
        }
    }

    /// Field names matching is not enough: the catalog also claims which
    /// fields may be omitted. Drop each field from a complete sample and see
    /// whether serde reports it missing, so `required` means structurally
    /// required and nothing else does. `required_when` is a validation rule
    /// applied after deserialization, so it is omittable here.
    #[test]
    fn catalog_requiredness_matches_what_serde_enforces() {
        for step in every_step() {
            let kind = step.kind();
            let spec =
                step_spec(kind).unwrap_or_else(|| panic!("{kind} is missing a catalog spec"));
            let complete = serde_json::to_value(&step).expect("step serializes");
            for field in spec.fields {
                let mut reduced = complete.clone();
                reduced
                    .as_object_mut()
                    .expect("object")
                    .remove(field.name)
                    .unwrap_or_else(|| panic!("{kind}.{} is present in the sample", field.name));
                let error = serde_json::from_value::<Step>(reduced).err();
                let serde_requires = error.is_some_and(|error| {
                    error
                        .to_string()
                        .contains(&format!("missing field `{}`", field.name))
                });
                let catalog_requires = field.requirement == Requirement::Required;
                assert_eq!(
                    catalog_requires,
                    serde_requires,
                    "`{kind}.{}` is described as `{}` but serde {} it",
                    field.name,
                    field.requirement.as_str(),
                    if serde_requires {
                        "structurally requires"
                    } else {
                        "accepts the file without"
                    }
                );
            }
        }
    }

    #[test]
    fn wait_condition_variants_match_the_catalog() {
        let object = object_spec("wait_condition").expect("wait_condition object");
        let described: BTreeSet<&str> = object
            .variants
            .iter()
            .map(|variant| variant.tag_value)
            .collect();
        let actual: BTreeSet<&str> = [
            WaitCondition::HeightAtLeast { height: 204 },
            WaitCondition::MempoolTxsAtLeast { count: 1 },
            WaitCondition::MempoolTxsAtMost { count: 1 },
            WaitCondition::Component {
                expected: full_expectation(),
            },
        ]
        .iter()
        .map(WaitCondition::kind)
        .collect();
        assert_eq!(described, actual);

        for condition in [
            WaitCondition::HeightAtLeast { height: 204 },
            WaitCondition::MempoolTxsAtLeast { count: 1 },
            WaitCondition::MempoolTxsAtMost { count: 1 },
            WaitCondition::Component {
                expected: full_expectation(),
            },
        ] {
            let kind = condition.kind();
            let variant = object
                .variants
                .iter()
                .find(|variant| variant.tag_value == kind)
                .unwrap_or_else(|| panic!("{kind} variant"));
            let described: BTreeSet<String> = variant
                .fields
                .iter()
                .map(|field| field.name.to_string())
                .collect();
            let serialized = serde_json::to_value(&condition).expect("condition serializes");
            let actual: BTreeSet<String> = serialized
                .as_object()
                .expect("object")
                .keys()
                .filter(|key| *key != "kind")
                .cloned()
                .collect();
            assert_eq!(
                described, actual,
                "catalog fields for wait condition `{kind}` do not match serde"
            );
        }
    }

    #[test]
    fn faucet_output_fields_match_the_catalog() {
        let object = object_spec("faucet_output").expect("faucet_output object");
        let described: BTreeSet<String> = object
            .fields
            .iter()
            .map(|field| field.name.to_string())
            .collect();
        let serialized = serde_json::to_value(FaucetScenarioOutput {
            address: Some("bcrt1qexample".to_string()),
            address_env: Some("FUND_ADD_1".to_string()),
            amount: "1btc".to_string(),
        })
        .expect("faucet output serializes");
        assert_eq!(described, serialized_keys(&serialized));
    }

    #[test]
    fn object_references_resolve() {
        let referenced = STEP_CATALOG
            .iter()
            .flat_map(|spec| spec.fields.iter())
            .chain(OBJECT_CATALOG.iter().flat_map(|object| {
                object.fields.iter().chain(
                    object
                        .variants
                        .iter()
                        .flat_map(|variant| variant.fields.iter()),
                )
            }))
            .filter_map(|field| field.value_type.object());
        for name in referenced {
            assert!(
                object_spec(name).is_some(),
                "field references unknown object `{name}`"
            );
        }
    }

    #[test]
    fn every_field_is_documented() {
        let fields = STEP_CATALOG
            .iter()
            .flat_map(|spec| spec.fields.iter())
            .chain(OBJECT_CATALOG.iter().flat_map(|object| {
                object.fields.iter().chain(
                    object
                        .variants
                        .iter()
                        .flat_map(|variant| variant.fields.iter()),
                )
            }));
        for field in fields {
            assert!(!field.name.is_empty(), "a field has no name");
            assert!(!field.help.is_empty(), "field `{}` has no help", field.name);
        }
        for spec in STEP_CATALOG {
            assert!(!spec.summary.is_empty(), "`{}` has no summary", spec.kind);
        }
    }

    /// The committed reference is generated output, not prose maintained by
    /// hand. Adding a step or renaming a field fails here until
    /// `simchainctl scenario schema --markdown` is re-run into the doc.
    #[test]
    fn committed_reference_matches_the_catalog() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/SCENARIOS.md")
            .canonicalize()
            .expect("docs/SCENARIOS.md exists");
        let doc = std::fs::read_to_string(&path).expect("read docs/SCENARIOS.md");
        let start = doc
            .find(REFERENCE_BEGIN)
            .expect("docs/SCENARIOS.md has a generated reference begin marker")
            + REFERENCE_BEGIN.len();
        let end = doc
            .find(REFERENCE_END)
            .expect("docs/SCENARIOS.md has a generated reference end marker");
        assert!(start < end, "generated reference markers are out of order");
        assert_eq!(
            doc[start..end].trim(),
            reference_markdown().trim(),
            "docs/SCENARIOS.md is stale; regenerate it with \
             `cargo run -p simchainctl -- scenario schema --markdown`"
        );
    }

    /// `all-features-live.yml` is the shipped demonstration of the whole
    /// language, so a new step type is not finished until that file shows it.
    #[test]
    fn all_features_scenario_demonstrates_every_step() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scenarios/all-features-live.yml")
            .canonicalize()
            .expect("scenarios/all-features-live.yml exists");
        let scenario = crate::schema::Scenario::parse(
            &std::fs::read_to_string(&path).expect("read all-features-live.yml"),
        )
        .expect("all-features-live.yml parses");
        scenario
            .validate()
            .expect("all-features-live.yml is a valid scenario");
        let demonstrated: BTreeSet<&str> = scenario.steps.iter().map(Step::kind).collect();
        let described: BTreeSet<&str> = STEP_CATALOG.iter().map(|spec| spec.kind).collect();
        let missing: Vec<&&str> = described.difference(&demonstrated).collect();
        assert!(
            missing.is_empty(),
            "scenarios/all-features-live.yml never uses: {missing:?}"
        );
    }

    /// A required field with a default would be contradictory, and an
    /// `exactly_one_of` member with a default could never be absent.
    #[test]
    fn requirements_and_defaults_are_consistent() {
        let fields = STEP_CATALOG
            .iter()
            .flat_map(|spec| spec.fields.iter().map(move |field| (spec.kind, field)));
        for (kind, field) in fields {
            match field.requirement {
                Requirement::Required | Requirement::RequiredWhen(_) => assert!(
                    field.default.is_none(),
                    "`{kind}.{}` is required but declares a default",
                    field.name
                ),
                Requirement::ExactlyOneOf(_) => assert!(
                    field.default.is_none(),
                    "`{kind}.{}` is one of a mutually exclusive pair but declares a default",
                    field.name
                ),
                Requirement::AtLeastOneOf(_) | Requirement::Optional => {}
            }
        }
    }
}
