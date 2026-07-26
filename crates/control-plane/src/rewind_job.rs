//! Coordinated true shorter-chain rewind across all three Bitcoin nodes.
//!
//! `invalidateblock` is node-local administrative state, so a shorter chain
//! only becomes the common active chain when the same boundary is invalidated
//! on node2, node3, and node1. The coordinator persists enough information
//! before the first mutation to either preserve a completed rewind or restore
//! the original branch after a failure or process restart.

use crate::state::ControlPlaneConfig;
use bitcoincore_rpc::RpcApi;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use simchain_common::control_api::RewindJobRequest;
use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

pub const MAX_REWIND_BLOCKS: u64 = 100;
pub const MIN_REWIND_TARGET_HEIGHT: u64 = 204;
pub const EXPLORER_RECOVERY_WARNING_CODE: &str = "electrs_reindex_may_be_required";
pub const EXPLORER_RECOVERY_COMMAND: &str = "./scripts/recover-explorer.sh";
pub const EXPLORER_RECOVERY_MESSAGE: &str = "If an electrs-based profile is active, its disposable index may need rebuilding after this rollback-only rewind. Bitcoin Core and its mempools are unaffected.";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindNode {
    Node1,
    Node2,
    Node3,
}

impl RewindNode {
    pub const ALL: [Self; 3] = [Self::Node1, Self::Node2, Self::Node3];
    pub const INVALIDATION_ORDER: [Self; 3] = [Self::Node2, Self::Node3, Self::Node1];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node1 => "node1",
            Self::Node2 => "node2",
            Self::Node3 => "node3",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RewindNodeTip {
    pub height: u64,
    pub best_hash: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindNodeState {
    #[default]
    Pending,
    Invalidated,
    Restored,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RewindRecoveryContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<RewindJobRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_tip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_hash: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_node_state: BTreeMap<RewindNode, RewindNodeState>,
    #[serde(default)]
    pub resolved: bool,
}

impl RewindRecoveryContext {
    fn prepared(
        request: RewindJobRequest,
        original: &RewindNodeTip,
        target_height: u64,
        target_tip: String,
        boundary_hash: String,
    ) -> Self {
        Self {
            request: Some(request),
            original_height: Some(original.height),
            original_tip: Some(original.best_hash.clone()),
            target_height: Some(target_height),
            target_tip: Some(target_tip),
            boundary_hash: Some(boundary_hash),
            per_node_state: RewindNode::ALL
                .into_iter()
                .map(|node| (node, RewindNodeState::Pending))
                .collect(),
            resolved: false,
        }
    }

    fn required(&self) -> anyhow::Result<RequiredRecovery<'_>> {
        Ok(RequiredRecovery {
            original_height: self
                .original_height
                .ok_or_else(|| anyhow::anyhow!("rewind original height is missing"))?,
            original_tip: self
                .original_tip
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("rewind original tip is missing"))?,
            target_height: self
                .target_height
                .ok_or_else(|| anyhow::anyhow!("rewind target height is missing"))?,
            target_tip: self
                .target_tip
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("rewind target tip is missing"))?,
            boundary_hash: self
                .boundary_hash
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("rewind boundary hash is missing"))?,
        })
    }
}

struct RequiredRecovery<'a> {
    original_height: u64,
    original_tip: &'a str,
    target_height: u64,
    target_tip: &'a str,
    boundary_hash: &'a str,
}

#[derive(Debug)]
pub struct RewindExecution {
    pub result: Value,
    pub chain_changed: bool,
    pub aborted: bool,
}

pub trait RewindObserver: Send + Sync {
    /// Persist the full recovery context synchronously. Returning success is a
    /// precondition for issuing the next chain mutation.
    fn persist(&self, context: &RewindRecoveryContext) -> anyhow::Result<()>;
    fn progress(&self, phase: &str, message: &str, data: Option<Value>);
    fn abort_requested(&self) -> bool;
}

pub trait RewindBackend: Send + Sync {
    fn tips(&self) -> anyhow::Result<BTreeMap<RewindNode, RewindNodeTip>>;
    fn block_hash(&self, node: RewindNode, height: u64) -> anyhow::Result<String>;
    fn invalidate_block(&self, node: RewindNode, hash: &str) -> anyhow::Result<()>;
    fn reconsider_block(&self, node: RewindNode, hash: &str) -> anyhow::Result<()>;
    fn mempool_size(&self, node: RewindNode) -> anyhow::Result<u64>;
    fn wait(&self, duration: Duration);
}

pub trait RewindExecutor: Send + Sync {
    fn execute(
        &self,
        request: &RewindJobRequest,
        observer: &dyn RewindObserver,
    ) -> anyhow::Result<RewindExecution>;

    fn recover(
        &self,
        context: &RewindRecoveryContext,
        observer: &dyn RewindObserver,
    ) -> anyhow::Result<()>;
}

pub struct CoordinatedRewindExecutor {
    backend: Box<dyn RewindBackend>,
}

impl CoordinatedRewindExecutor {
    pub fn new(backend: Box<dyn RewindBackend>) -> Self {
        Self { backend }
    }

    fn common_tip(&self) -> anyhow::Result<RewindNodeTip> {
        let tips = self.backend.tips()?;
        let node1 = tips
            .get(&RewindNode::Node1)
            .ok_or_else(|| anyhow::anyhow!("node1 tip is missing"))?;
        for node in RewindNode::ALL {
            let tip = tips
                .get(&node)
                .ok_or_else(|| anyhow::anyhow!("{} tip is missing", node.as_str()))?;
            anyhow::ensure!(
                tip == node1,
                "rewind_precondition_failed: nodes are not converged (node1={} {}, {}={} {})",
                node1.height,
                node1.best_hash,
                node.as_str(),
                tip.height,
                tip.best_hash
            );
        }
        Ok(node1.clone())
    }

    fn tips_equal(&self, height: u64, hash: &str) -> anyhow::Result<bool> {
        Ok(self
            .backend
            .tips()?
            .values()
            .all(|tip| tip.height == height && tip.best_hash == hash))
    }

    fn wait_for_exact_tip(&self, height: u64, hash: &str) -> anyhow::Result<()> {
        for _ in 0..40 {
            if self.tips_equal(height, hash)? {
                return Ok(());
            }
            self.backend.wait(Duration::from_millis(250));
        }
        anyhow::bail!(
            "rewind_convergence_failed: nodes did not converge at height {height} hash {hash}"
        )
    }

    fn common_chain_contains(&self, height: u64, hash: &str) -> anyhow::Result<bool> {
        let tips = self.backend.tips()?;
        let Some(first) = tips.values().next() else {
            return Ok(false);
        };
        if tips.values().any(|tip| tip != first) || first.height < height {
            return Ok(false);
        }
        for node in RewindNode::ALL {
            if self.backend.block_hash(node, height)? != hash {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn rollback_once(
        &self,
        context: &mut RewindRecoveryContext,
        observer: &dyn RewindObserver,
    ) -> anyhow::Result<()> {
        let required = context.required()?;
        let boundary_hash = required.boundary_hash.to_string();
        let original_height = required.original_height;
        let original_tip = required.original_tip.to_string();
        observer.progress(
            "rolling_back",
            "restoring the original branch on all three nodes",
            Some(json!({"boundary_hash": boundary_hash})),
        );
        for node in RewindNode::ALL {
            self.backend.reconsider_block(node, &boundary_hash)?;
            context
                .per_node_state
                .insert(node, RewindNodeState::Restored);
            observer.persist(context)?;
        }
        for _ in 0..40 {
            if self.common_chain_contains(original_height, &original_tip)? {
                context.resolved = true;
                observer.persist(context)?;
                observer.progress(
                    "rolled_back",
                    "rewind rollback restored a common descendant of the original tip",
                    Some(json!({
                        "original_height": original_height,
                        "original_tip": original_tip
                    })),
                );
                return Ok(());
            }
            self.backend.wait(Duration::from_millis(250));
        }
        anyhow::bail!("rewind_rollback_failed: nodes did not restore the original branch")
    }

    fn rollback_until_safe(
        &self,
        context: &mut RewindRecoveryContext,
        observer: &dyn RewindObserver,
    ) {
        let mut attempts = 0u64;
        loop {
            attempts += 1;
            match self.rollback_once(context, observer) {
                Ok(()) => return,
                Err(error) => {
                    if attempts == 1 || attempts.is_multiple_of(15) {
                        observer.progress(
                            "rollback_pending",
                            &format!(
                                "rewind recovery remains pending (attempt {attempts}): {error}"
                            ),
                            Some(json!({"attempt": attempts})),
                        );
                    }
                    self.backend.wait(Duration::from_secs(2));
                }
            }
        }
    }

    fn completed_rewind_is_safe(&self, context: &RewindRecoveryContext) -> anyhow::Result<bool> {
        let required = context.required()?;
        if self.tips_equal(required.target_height, required.target_tip)? {
            return Ok(true);
        }
        let tips = self.backend.tips()?;
        let Some(common) = tips.values().next() else {
            return Ok(false);
        };
        if tips.values().any(|tip| tip != common)
            || common.height < required.target_height
            || !self.common_chain_contains(required.target_height, required.target_tip)?
        {
            return Ok(false);
        }
        if common.height < required.original_height {
            return Ok(true);
        }
        Ok(!self.common_chain_contains(required.original_height, required.original_tip)?)
    }
}

impl RewindExecutor for CoordinatedRewindExecutor {
    fn execute(
        &self,
        request: &RewindJobRequest,
        observer: &dyn RewindObserver,
    ) -> anyhow::Result<RewindExecution> {
        anyhow::ensure!(
            (1..=MAX_REWIND_BLOCKS).contains(&request.blocks),
            "rewind blocks must be between 1 and {MAX_REWIND_BLOCKS}"
        );
        let original = self.common_tip()?;
        let target_height = original.height.checked_sub(request.blocks).ok_or_else(|| {
            anyhow::anyhow!("rewind_precondition_failed: rewind exceeds current height")
        })?;
        anyhow::ensure!(
            target_height >= MIN_REWIND_TARGET_HEIGHT,
            "rewind_precondition_failed: target height {target_height} is below bootstrap floor {MIN_REWIND_TARGET_HEIGHT}"
        );
        let target_tip = self.backend.block_hash(RewindNode::Node2, target_height)?;
        let boundary_height = target_height + 1;
        let boundary_hash = self
            .backend
            .block_hash(RewindNode::Node2, boundary_height)?;
        let mut context = RewindRecoveryContext::prepared(
            request.clone(),
            &original,
            target_height,
            target_tip.clone(),
            boundary_hash.clone(),
        );
        observer.persist(&context)?;
        observer.progress(
            "prepared",
            &format!(
                "prepared rewind from height {} to {target_height}",
                original.height
            ),
            Some(json!({
                "original_height": original.height,
                "original_tip": original.best_hash,
                "target_height": target_height,
                "target_tip": target_tip,
                "boundary_hash": boundary_hash
            })),
        );

        if observer.abort_requested() {
            context.resolved = true;
            observer.persist(&context)?;
            return Ok(RewindExecution {
                result: json!({"rewound_blocks": 0, "aborted": true}),
                chain_changed: false,
                aborted: true,
            });
        }

        let mut chain_changed = false;
        for node in RewindNode::INVALIDATION_ORDER {
            if observer.abort_requested() {
                self.rollback_until_safe(&mut context, observer);
                return Ok(RewindExecution {
                    result: json!({
                        "rewound_blocks": 0,
                        "original_height": original.height,
                        "original_tip": original.best_hash,
                        "aborted": true,
                        "rolled_back": true
                    }),
                    chain_changed,
                    aborted: true,
                });
            }
            observer.progress(
                "invalidating",
                &format!("invalidating rewind boundary on {}", node.as_str()),
                Some(json!({"node": node, "boundary_hash": boundary_hash})),
            );
            chain_changed = true;
            let mutation = self.backend.invalidate_block(node, &boundary_hash);
            if let Err(primary) = mutation {
                self.rollback_until_safe(&mut context, observer);
                anyhow::bail!(
                    "rewind_invalidation_failed on {} (original chain restored): {primary}",
                    node.as_str()
                );
            }
            let verified = (|| -> anyhow::Result<()> {
                let tip = self
                    .backend
                    .tips()?
                    .remove(&node)
                    .ok_or_else(|| anyhow::anyhow!("{} tip disappeared", node.as_str()))?;
                anyhow::ensure!(
                    tip.height == target_height && tip.best_hash == target_tip,
                    "{} stopped at height {} hash {} instead of height {target_height} hash {target_tip}",
                    node.as_str(),
                    tip.height,
                    tip.best_hash
                );
                context
                    .per_node_state
                    .insert(node, RewindNodeState::Invalidated);
                observer.persist(&context)
            })();
            if let Err(primary) = verified {
                self.rollback_until_safe(&mut context, observer);
                anyhow::bail!(
                    "rewind_invalidation_failed while verifying {} (original chain restored): {primary}",
                    node.as_str(),
                );
            }
            observer.progress(
                "invalidated",
                &format!("{} reached the rewind target", node.as_str()),
                Some(json!({"node": node, "height": target_height, "hash": target_tip})),
            );
        }

        if observer.abort_requested() {
            self.rollback_until_safe(&mut context, observer);
            return Ok(RewindExecution {
                result: json!({
                    "rewound_blocks": 0,
                    "original_height": original.height,
                    "original_tip": original.best_hash,
                    "aborted": true,
                    "rolled_back": true
                }),
                chain_changed,
                aborted: true,
            });
        }

        if let Err(primary) = self.wait_for_exact_tip(target_height, &target_tip) {
            self.rollback_until_safe(&mut context, observer);
            anyhow::bail!("{primary} (original chain restored)");
        }

        context.resolved = true;
        observer.persist(&context)?;
        let tips = self.backend.tips()?;
        let mut nodes = serde_json::Map::new();
        for node in RewindNode::ALL {
            let tip = tips
                .get(&node)
                .ok_or_else(|| anyhow::anyhow!("{} final tip is missing", node.as_str()))?;
            nodes.insert(
                node.as_str().to_string(),
                json!({
                    "height": tip.height,
                    "best_hash": tip.best_hash,
                    "mempool_size": self.backend.mempool_size(node)?
                }),
            );
        }
        Ok(RewindExecution {
            result: json!({
                "rewound_blocks": request.blocks,
                "original_height": original.height,
                "original_tip": original.best_hash,
                "final_height": target_height,
                "final_tip": target_tip,
                "boundary_hash": boundary_hash,
                "nodes": nodes,
                "mining_desired_state_changed": false,
                "warnings": [{
                    "code": EXPLORER_RECOVERY_WARNING_CODE,
                    "message": EXPLORER_RECOVERY_MESSAGE,
                    "affected_component": "electrs",
                    "recovery_command": EXPLORER_RECOVERY_COMMAND
                }]
            }),
            chain_changed,
            aborted: false,
        })
    }

    fn recover(
        &self,
        context: &RewindRecoveryContext,
        observer: &dyn RewindObserver,
    ) -> anyhow::Result<()> {
        if context.boundary_hash.is_none() {
            return Ok(());
        }
        if self.completed_rewind_is_safe(context)? {
            let mut resolved = context.clone();
            resolved.resolved = true;
            observer.persist(&resolved)?;
            observer.progress(
                "recovery_complete",
                "interrupted rewind already has a safe common rewound tip",
                None,
            );
            return Ok(());
        }
        let mut rollback = context.clone();
        self.rollback_once(&mut rollback, observer)
    }
}

pub struct RpcRewindBackend {
    node_urls: BTreeMap<RewindNode, String>,
    node1_internal_user: String,
    node1_internal_pass: String,
}

impl RpcRewindBackend {
    pub fn from_config(config: &ControlPlaneConfig) -> Self {
        Self {
            node_urls: BTreeMap::from([
                (RewindNode::Node1, config.node1_url.clone()),
                (RewindNode::Node2, config.node2_url.clone()),
                (RewindNode::Node3, config.node3_url.clone()),
            ]),
            node1_internal_user: config.node1_internal_rpc_user.clone(),
            node1_internal_pass: config.node1_internal_rpc_pass.clone(),
        }
    }

    fn client(&self, node: RewindNode) -> anyhow::Result<bitcoincore_rpc::Client> {
        let url = self
            .node_urls
            .get(&node)
            .ok_or_else(|| anyhow::anyhow!("{} RPC URL is missing", node.as_str()))?;
        if node == RewindNode::Node1 {
            Ok(simchain_common::create_client_with_auth(
                url,
                self.node1_internal_user.clone(),
                self.node1_internal_pass.clone(),
            )?)
        } else {
            Ok(simchain_common::create_client(url)?)
        }
    }
}

impl RewindBackend for RpcRewindBackend {
    fn tips(&self) -> anyhow::Result<BTreeMap<RewindNode, RewindNodeTip>> {
        RewindNode::ALL
            .into_iter()
            .map(|node| {
                let client = self.client(node)?;
                Ok((
                    node,
                    RewindNodeTip {
                        height: client.get_block_count()?,
                        best_hash: client.get_best_block_hash()?.to_string(),
                    },
                ))
            })
            .collect()
    }

    fn block_hash(&self, node: RewindNode, height: u64) -> anyhow::Result<String> {
        Ok(self.client(node)?.get_block_hash(height)?.to_string())
    }

    fn invalidate_block(&self, node: RewindNode, hash: &str) -> anyhow::Result<()> {
        self.client(node)?
            .call::<Value>("invalidateblock", &[json!(hash)])?;
        Ok(())
    }

    fn reconsider_block(&self, node: RewindNode, hash: &str) -> anyhow::Result<()> {
        self.client(node)?
            .call::<Value>("reconsiderblock", &[json!(hash)])?;
        Ok(())
    }

    fn mempool_size(&self, node: RewindNode) -> anyhow::Result<u64> {
        let entries = self
            .client(node)?
            .call::<Vec<String>>("getrawmempool", &[])?;
        Ok(entries.len() as u64)
    }

    fn wait(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct MockChain {
        active: BTreeMap<RewindNode, Vec<String>>,
        removed: Vec<String>,
        calls: Vec<String>,
        fail_invalidate_once: Option<RewindNode>,
    }

    struct MockBackend {
        chain: Mutex<MockChain>,
    }

    impl MockBackend {
        fn converged(height: u64) -> Self {
            let blocks: Vec<String> = (0..=height).map(|n| format!("block-{n}")).collect();
            Self {
                chain: Mutex::new(MockChain {
                    active: RewindNode::ALL
                        .into_iter()
                        .map(|node| (node, blocks.clone()))
                        .collect(),
                    removed: Vec::new(),
                    calls: Vec::new(),
                    fail_invalidate_once: None,
                }),
            }
        }
    }

    impl RewindBackend for std::sync::Arc<MockBackend> {
        fn tips(&self) -> anyhow::Result<BTreeMap<RewindNode, RewindNodeTip>> {
            let chain = self.chain.lock().expect("mock chain");
            Ok(chain
                .active
                .iter()
                .map(|(node, blocks)| {
                    (
                        *node,
                        RewindNodeTip {
                            height: blocks.len() as u64 - 1,
                            best_hash: blocks.last().expect("tip").clone(),
                        },
                    )
                })
                .collect())
        }

        fn block_hash(&self, node: RewindNode, height: u64) -> anyhow::Result<String> {
            Ok(self.chain.lock().expect("mock chain").active[&node][height as usize].clone())
        }

        fn invalidate_block(&self, node: RewindNode, hash: &str) -> anyhow::Result<()> {
            let mut chain = self.chain.lock().expect("mock chain");
            chain.calls.push(format!("invalidate:{}", node.as_str()));
            if chain.fail_invalidate_once == Some(node) {
                chain.fail_invalidate_once = None;
                anyhow::bail!("injected invalidation failure");
            }
            let blocks = chain.active.get(&node).expect("node blocks");
            let index = blocks
                .iter()
                .position(|block| block == hash)
                .expect("boundary");
            let removed = blocks[index..].to_vec();
            chain.removed = removed;
            chain
                .active
                .get_mut(&node)
                .expect("node blocks")
                .truncate(index);
            Ok(())
        }

        fn reconsider_block(&self, node: RewindNode, _hash: &str) -> anyhow::Result<()> {
            let mut chain = self.chain.lock().expect("mock chain");
            chain.calls.push(format!("reconsider:{}", node.as_str()));
            let removed = chain.removed.clone();
            let blocks = chain.active.get_mut(&node).expect("node blocks");
            if blocks.last().is_some_and(|tip| tip == "block-210") {
                return Ok(());
            }
            for block in removed {
                if !blocks.contains(&block) {
                    blocks.push(block);
                }
            }
            Ok(())
        }

        fn mempool_size(&self, _node: RewindNode) -> anyhow::Result<u64> {
            Ok(0)
        }

        fn wait(&self, _duration: Duration) {}
    }

    #[derive(Default)]
    struct Observer {
        contexts: Mutex<Vec<RewindRecoveryContext>>,
        abort: AtomicBool,
        abort_after_node: Option<RewindNode>,
        fail_persist_at: Mutex<Option<usize>>,
    }

    impl RewindObserver for Observer {
        fn persist(&self, context: &RewindRecoveryContext) -> anyhow::Result<()> {
            let mut contexts = self.contexts.lock().expect("contexts");
            let call = contexts.len() + 1;
            let mut failure = self.fail_persist_at.lock().expect("persist failure");
            if *failure == Some(call) {
                *failure = None;
                anyhow::bail!("injected persistence failure");
            }
            contexts.push(context.clone());
            Ok(())
        }

        fn progress(&self, phase: &str, _message: &str, data: Option<Value>) {
            if phase == "invalidated" {
                let actual = data
                    .as_ref()
                    .and_then(|value| value.get("node"))
                    .and_then(Value::as_str);
                if self.abort_after_node.map(RewindNode::as_str) == actual {
                    self.abort.store(true, Ordering::Release);
                }
            }
        }

        fn abort_requested(&self) -> bool {
            self.abort.load(Ordering::Acquire)
        }
    }

    #[test]
    fn successful_rewind_invalidates_miners_then_user_node() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let observer = Observer::default();
        let result = executor
            .execute(&RewindJobRequest { blocks: 3 }, &observer)
            .expect("rewind");
        assert_eq!(result.result["final_height"], 207);
        assert_eq!(
            result.result["warnings"][0]["code"],
            EXPLORER_RECOVERY_WARNING_CODE
        );
        assert_eq!(
            result.result["warnings"][0]["recovery_command"],
            EXPLORER_RECOVERY_COMMAND
        );
        assert_eq!(
            backend.chain.lock().expect("chain").calls,
            ["invalidate:node2", "invalidate:node3", "invalidate:node1"]
        );
        assert!(
            observer
                .contexts
                .lock()
                .expect("contexts")
                .last()
                .expect("context")
                .resolved
        );
    }

    #[test]
    fn failure_after_first_node_restores_original_chain() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        backend.chain.lock().expect("chain").fail_invalidate_once = Some(RewindNode::Node3);
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let error = executor
            .execute(&RewindJobRequest { blocks: 2 }, &Observer::default())
            .expect_err("injected failure");
        assert!(error.to_string().contains("original chain restored"));
        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 210 && tip.best_hash == "block-210"));
    }

    #[test]
    fn abort_after_first_node_restores_original_chain() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let observer = Observer {
            abort_after_node: Some(RewindNode::Node2),
            ..Observer::default()
        };
        let execution = executor
            .execute(&RewindJobRequest { blocks: 2 }, &observer)
            .expect("safe abort");
        assert!(execution.aborted);
        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 210 && tip.best_hash == "block-210"));
    }

    #[test]
    fn abort_after_last_node_still_restores_original_chain() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let observer = Observer {
            abort_after_node: Some(RewindNode::Node1),
            ..Observer::default()
        };
        let execution = executor
            .execute(&RewindJobRequest { blocks: 2 }, &observer)
            .expect("safe late abort");
        assert!(execution.aborted);
        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 210 && tip.best_hash == "block-210"));
    }

    #[test]
    fn persistence_failure_after_mutation_restores_original_chain() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let observer = Observer {
            fail_persist_at: Mutex::new(Some(2)),
            ..Observer::default()
        };
        let error = executor
            .execute(&RewindJobRequest { blocks: 2 }, &observer)
            .expect_err("persist failure");
        assert!(error.to_string().contains("original chain restored"));
        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 210 && tip.best_hash == "block-210"));
    }

    #[test]
    fn restart_recovery_restores_a_partially_rewound_chain() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let original = RewindNodeTip {
            height: 210,
            best_hash: "block-210".to_string(),
        };
        let mut context = RewindRecoveryContext::prepared(
            RewindJobRequest { blocks: 2 },
            &original,
            208,
            "block-208".to_string(),
            "block-209".to_string(),
        );
        backend
            .invalidate_block(RewindNode::Node2, "block-209")
            .expect("partial invalidation");
        context
            .per_node_state
            .insert(RewindNode::Node2, RewindNodeState::Invalidated);

        let observer = Observer::default();
        executor
            .recover(&context, &observer)
            .expect("restart recovery");

        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 210 && tip.best_hash == "block-210"));
        assert!(
            observer
                .contexts
                .lock()
                .expect("contexts")
                .last()
                .expect("resolved recovery context")
                .resolved
        );
    }

    #[test]
    fn restart_recovery_preserves_an_already_completed_rewind() {
        let backend = std::sync::Arc::new(MockBackend::converged(210));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend.clone()));
        let original = RewindNodeTip {
            height: 210,
            best_hash: "block-210".to_string(),
        };
        let mut context = RewindRecoveryContext::prepared(
            RewindJobRequest { blocks: 2 },
            &original,
            208,
            "block-208".to_string(),
            "block-209".to_string(),
        );
        for node in RewindNode::INVALIDATION_ORDER {
            backend
                .invalidate_block(node, "block-209")
                .expect("completed invalidation");
            context
                .per_node_state
                .insert(node, RewindNodeState::Invalidated);
        }

        let observer = Observer::default();
        executor
            .recover(&context, &observer)
            .expect("completed rewind recovery");

        assert!(backend
            .tips()
            .expect("tips")
            .values()
            .all(|tip| tip.height == 208 && tip.best_hash == "block-208"));
        assert!(!backend
            .chain
            .lock()
            .expect("chain")
            .calls
            .iter()
            .any(|call| call.starts_with("reconsider:")));
        assert!(
            observer
                .contexts
                .lock()
                .expect("contexts")
                .last()
                .expect("resolved recovery context")
                .resolved
        );
    }

    #[test]
    fn rejects_target_below_bootstrap_floor() {
        let backend = std::sync::Arc::new(MockBackend::converged(205));
        let executor = CoordinatedRewindExecutor::new(Box::new(backend));
        let error = executor
            .execute(&RewindJobRequest { blocks: 2 }, &Observer::default())
            .expect_err("bootstrap floor");
        assert!(error.to_string().contains("below bootstrap floor"));
    }
}
