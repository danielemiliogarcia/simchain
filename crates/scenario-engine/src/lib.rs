//! Pure declarative scenario schema and ordered execution engine.
//!
//! The library owns validation and sequencing only. Concrete actions are
//! supplied by the control plane; it has no Docker, process, filesystem, or
//! Bitcoin RPC backend of its own.

mod catalog;
mod engine;
mod results;
mod schema;

pub use catalog::{
    object_spec, reference_markdown, schema_response, step_spec, FieldSpec, FieldType, ObjectSpec,
    Requirement, StepRequires, StepSpec, VariantSpec, CATALOG_BOOTSTRAP_HEIGHT, OBJECT_CATALOG,
    REFERENCE_BEGIN, REFERENCE_END, SCENARIO_VERSION, STEP_CATALOG,
};
pub use engine::{run, ScenarioActions, ScenarioControl, ScenarioProgress, ScenarioProgressPhase};
pub use results::{ScenarioResult, ScenarioStepResult};
pub use schema::{
    CheckpointStep, ComponentExpectation, FaucetScenarioOutput, MinerNode, NetworkNode, Scenario,
    ScenarioComponent, Step, TxWaitState, WaitCondition, WaitTxStep, BOOTSTRAP_HEIGHT,
};
