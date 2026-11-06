use crate::*;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub slots: u32,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub disk_bytes: u64,
}
impl Resources {
    fn fits(self, capacity: Self) -> bool {
        self.slots <= capacity.slots
            && self.memory_bytes <= capacity.memory_bytes
            && self.cpu_millis <= capacity.cpu_millis
            && self.disk_bytes <= capacity.disk_bytes
    }
    fn reserve(&mut self, value: Self) {
        self.slots -= value.slots;
        self.memory_bytes -= value.memory_bytes;
        self.cpu_millis -= value.cpu_millis;
        self.disk_bytes -= value.disk_bytes;
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeCapacity {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub available: Resources,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeEligibility {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub eligible: bool,
    pub source_warm: bool,
    pub reason_codes: Vec<Label>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementCandidate {
    pub project_id: Uuid,
    pub task: TaskRevision,
    pub selection: ArtifactRef,
    pub priority: i32,
    pub queued_at_ms: u64,
    pub expires_at_ms: u64,
    pub resources: Resources,
    pub nodes: Vec<NodeEligibility>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementInput {
    pub schema_version: u32,
    pub now_ms: u64,
    pub after_project: Option<Uuid>,
    pub max_offers: u32,
    pub candidates: Vec<PlacementCandidate>,
    pub nodes: Vec<NodeCapacity>,
}
impl PlacementInput {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 1 && self.now_ms > 0 && (1..=128).contains(&self.max_offers),
            "placement version/time/limit",
        )?;
        if let Some(id) = self.after_project {
            crate::id(id)?;
        }
        require(
            self.candidates.len() <= 256 && self.nodes.len() <= 128,
            "placement cardinality",
        )?;
        let mut nodes = BTreeSet::new();
        for n in &self.nodes {
            id(n.node_id)?;
            id(n.boot_id)?;
            revision(n.revision)?;
            time(n.observed_at_ms, n.expires_at_ms, 120_000)?;
            require(nodes.insert(n.node_id), "duplicate node capacity")?;
        }
        let mut tasks = BTreeSet::new();
        let mut projects = BTreeSet::new();
        for c in &self.candidates {
            id(c.project_id)?;
            c.task.validate()?;
            artifact(&c.selection)?;
            projects.insert(c.project_id);
            require(
                tasks.insert(c.task.task_id),
                "ambiguous duplicate graph task",
            )?;
            require(
                c.queued_at_ms > 0
                    && c.queued_at_ms <= self.now_ms
                    && c.expires_at_ms > c.queued_at_ms
                    && c.expires_at_ms
                        .checked_sub(self.now_ms)
                        .is_none_or(|remaining| remaining <= 30_000)
                    && c.resources.slots > 0
                    && c.resources.memory_bytes > 0
                    && c.resources.cpu_millis > 0
                    && c.nodes.len() <= 128,
                "candidate time/resources/bounds",
            )?;
            let mut eligibility = BTreeSet::new();
            for n in &c.nodes {
                id(n.node_id)?;
                id(n.boot_id)?;
                require(
                    n.reason_codes.len() <= 16 && eligibility.insert(n.node_id),
                    "duplicate/excess node eligibility",
                )?;
            }
        }
        require(projects.len() <= 64, "project cardinality")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposedPlacement {
    pub project_id: Uuid,
    pub task: TaskRevision,
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub capacity_revision: u64,
    pub selection: ArtifactRef,
    pub resources: Resources,
    pub source_warm: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementRefusal {
    pub task_id: Uuid,
    pub reason: Label,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementOutput {
    pub schema_version: u32,
    pub authority_admission: bool,
    pub next_after_project: Option<Uuid>,
    pub proposed: Vec<ProposedPlacement>,
    pub refused: Vec<PlacementRefusal>,
}
impl PlacementOutput {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 1
                && !self.authority_admission
                && self.proposed.len() <= 128
                && self.refused.len() <= 256,
            "proposal is not authority",
        )?;
        if let Some(project) = self.next_after_project {
            id(project)?;
        }
        let mut seen = BTreeSet::new();
        for p in &self.proposed {
            id(p.project_id)?;
            p.task.validate()?;
            id(p.node_id)?;
            id(p.boot_id)?;
            revision(p.capacity_revision)?;
            artifact(&p.selection)?;
            require(
                p.resources.slots > 0
                    && p.resources.memory_bytes > 0
                    && p.resources.cpu_millis > 0
                    && seen.insert(p.task.task_id),
                "duplicate/invalid placement",
            )?;
        }
        for r in &self.refused {
            id(r.task_id)?;
            require(seen.insert(r.task_id), "duplicate refusal/placement")?;
        }
        Ok(())
    }
}
/// Fair bounded proposal only. No DB claim is made; eligibility is an explicit
/// trusted-input fact, while warmth affects preference only AFTER eligibility.
pub fn place(input: &PlacementInput) -> Result<PlacementOutput> {
    input.validate()?;
    let mut capacity: BTreeMap<_, _> = input
        .nodes
        .iter()
        .map(|n| (n.node_id, (n, n.available)))
        .collect();
    let mut projects: BTreeMap<Uuid, Vec<&PlacementCandidate>> = BTreeMap::new();
    for c in &input.candidates {
        projects.entry(c.project_id).or_default().push(c);
    }
    for tasks in projects.values_mut() {
        tasks.sort_by_key(|c| {
            (
                std::cmp::Reverse(c.priority),
                c.queued_at_ms,
                c.task.task_id,
            )
        });
    }
    let mut order: Vec<_> = projects.keys().copied().collect();
    if let Some(after) = input.after_project {
        let index = order.partition_point(|p| *p <= after);
        order.rotate_left(index);
    }
    let mut result = PlacementOutput {
        schema_version: 1,
        authority_admission: false,
        next_after_project: input.after_project,
        proposed: vec![],
        refused: vec![],
    };
    let mut pending: BTreeSet<_> = input.candidates.iter().map(|c| c.task.task_id).collect();
    loop {
        let mut progress = false;
        for project in &order {
            if result.proposed.len() >= input.max_offers as usize {
                break;
            }
            let mut accepted = None;
            for c in &projects[project] {
                if !pending.contains(&c.task.task_id) {
                    continue;
                }
                if input.now_ms >= c.expires_at_ms {
                    pending.remove(&c.task.task_id);
                    result.refused.push(PlacementRefusal {
                        task_id: c.task.task_id,
                        reason: Label::new("candidate_stale")?,
                    });
                    continue;
                }
                let mut eligible: Vec<_> = c.nodes.iter().filter(|n| n.eligible).collect();
                eligible.sort_by_key(|n| (!n.source_warm, n.node_id));
                for n in eligible {
                    if let Some((snapshot, available)) = capacity.get_mut(&n.node_id)
                        && snapshot.boot_id == n.boot_id
                        && snapshot.observed_at_ms <= input.now_ms
                        && input.now_ms < snapshot.expires_at_ms
                        && c.resources.fits(*available)
                    {
                        available.reserve(c.resources);
                        accepted = Some((*c, n, snapshot.revision));
                        break;
                    }
                }
                if accepted.is_some() {
                    break;
                }
            }
            if let Some((c, n, revision)) = accepted {
                pending.remove(&c.task.task_id);
                result.proposed.push(ProposedPlacement {
                    project_id: *project,
                    task: c.task.clone(),
                    node_id: n.node_id,
                    boot_id: n.boot_id,
                    capacity_revision: revision,
                    selection: c.selection.clone(),
                    resources: c.resources,
                    source_warm: n.source_warm,
                });
                result.next_after_project = Some(*project);
                progress = true;
            }
        }
        if !progress || result.proposed.len() >= input.max_offers as usize {
            break;
        }
    }
    for task in pending {
        result.refused.push(PlacementRefusal {
            task_id: task,
            reason: Label::new(if result.proposed.len() >= input.max_offers as usize {
                "batch_limit"
            } else {
                "no_current_eligible_resource_fit"
            })?,
        });
    }
    result.refused.sort_by_key(|r| r.task_id);
    result.validate()?;
    Ok(result)
}
