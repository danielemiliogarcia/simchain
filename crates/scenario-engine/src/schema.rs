use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, fs, path::Path};

pub const BOOTSTRAP_HEIGHT: u64 = 204;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub version: u64,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    WaitHeight {
        height: u64,
    },
    Sleep {
        secs: u64,
    },
    PauseMining,
    ResumeMining,
    Mine {
        node: MinerNode,
        blocks: u64,
    },
    Reorg {
        depth: u64,
        #[serde(default)]
        empty: bool,
    },
    SpamBurst {
        node: MinerNode,
        txs: u64,
        outputs_per_tx: u64,
    },
    Partition {
        node: MinerNode,
        main_blocks: u64,
        isolated_blocks: u64,
    },
}

impl Step {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WaitHeight { .. } => "wait_height",
            Self::Sleep { .. } => "sleep",
            Self::PauseMining => "pause_mining",
            Self::ResumeMining => "resume_mining",
            Self::Mine { .. } => "mine",
            Self::Reorg { .. } => "reorg",
            Self::SpamBurst { .. } => "spam_burst",
            Self::Partition { .. } => "partition",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum MinerNode {
    #[serde(rename = "btc-simnet-node2")]
    Node2,
    #[serde(rename = "btc-simnet-node3")]
    Node3,
}

impl fmt::Display for MinerNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Node2 => "btc-simnet-node2",
            Self::Node3 => "btc-simnet-node3",
        })
    }
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read scenario file {}", path.display()))?;
        let scenario: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse scenario file {}", path.display()))?;
        scenario.validate()?;
        Ok(scenario)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported scenario version {}; expected 1", self.version);
        }
        for (index, step) in self.steps.iter().enumerate() {
            let error = match step {
                Step::WaitHeight { height } if *height < BOOTSTRAP_HEIGHT => {
                    Some(format!("height must be at least {BOOTSTRAP_HEIGHT}"))
                }
                Step::Sleep { secs } if *secs == 0 => Some("secs must be positive".to_string()),
                Step::Mine { blocks, .. } if *blocks == 0 => {
                    Some("blocks must be positive".to_string())
                }
                Step::Reorg { depth, .. } if *depth == 0 => {
                    Some("depth must be positive".to_string())
                }
                Step::SpamBurst { txs, .. } if *txs == 0 => {
                    Some("txs must be positive".to_string())
                }
                Step::Partition {
                    main_blocks,
                    isolated_blocks,
                    ..
                } if *main_blocks == 0 || *isolated_blocks == 0 => {
                    Some("main_blocks and isolated_blocks must be positive".to_string())
                }
                Step::Partition {
                    main_blocks,
                    isolated_blocks,
                    ..
                } if main_blocks == isolated_blocks => {
                    Some("main_blocks and isolated_blocks must differ".to_string())
                }
                _ => None,
            };
            if let Some(error) = error {
                bail!("invalid step {} ({}): {error}", index + 1, step.kind());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<Scenario> {
        let scenario: Scenario = serde_yaml::from_str(yaml)?;
        scenario.validate()?;
        Ok(scenario)
    }

    #[test]
    fn parses_valid_v1_and_preserves_order() {
        let scenario = parse(
            r#"
version: 1
steps:
  - type: wait_height
    height: 260
  - type: pause_mining
  - type: mine
    node: btc-simnet-node2
    blocks: 3
  - type: reorg
    depth: 2
    empty: false
"#,
        )
        .unwrap();
        let kinds: Vec<_> = scenario.steps.iter().map(Step::kind).collect();
        assert_eq!(kinds, ["wait_height", "pause_mining", "mine", "reorg"]);
    }

    #[test]
    fn rejects_unknown_version() {
        let error = parse("version: 2\nsteps: []\n").unwrap_err();
        assert!(error.to_string().contains("unsupported scenario version"));
    }

    #[test]
    fn rejects_invalid_step_fields() {
        let error = parse("version: 1\nsteps:\n  - type: sleep\n    secs: 0\n").unwrap_err();
        assert!(error.to_string().contains("secs must be positive"));

        let error =
            parse("version: 1\nsteps:\n  - type: wait_height\n    height: 203\n").unwrap_err();
        assert!(error.to_string().contains("height must be at least 204"));
    }

    #[test]
    fn rejects_equal_partition_block_counts() {
        let error = parse(
            "version: 1\nsteps:\n  - type: partition\n    node: btc-simnet-node3\n    main_blocks: 4\n    isolated_blocks: 4\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("must differ"));
    }

    #[test]
    fn rejects_unknown_miner() {
        let error = serde_yaml::from_str::<Scenario>(
            "version: 1\nsteps:\n  - type: mine\n    node: btc-simnet-node1\n    blocks: 1\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn shipped_scenarios_are_valid() {
        for yaml in [
            include_str!("../../../scenarios/pause-then-burst.yml"),
            include_str!("../../../scenarios/reorg-during-sync.yml"),
            include_str!("../../../scenarios/partition-node3.yml"),
        ] {
            parse(yaml).unwrap();
        }
    }
}
