//! Pure declarative scenario schema and ordered execution engine.
//!
//! The library owns validation and sequencing only. Concrete actions are
//! supplied by the control plane; it has no Docker, process, filesystem, or
//! Bitcoin RPC backend of its own.

mod engine;
mod results;
mod schema;

pub use engine::{run, ScenarioActions, ScenarioControl, ScenarioProgress, ScenarioProgressPhase};
pub use results::{ScenarioResult, ScenarioStepResult};
pub use schema::{
    CheckpointStep, ComponentExpectation, FaucetScenarioOutput, MinerNode, NetworkNode, Scenario,
    ScenarioComponent, Step, WaitCondition, BOOTSTRAP_HEIGHT,
};
