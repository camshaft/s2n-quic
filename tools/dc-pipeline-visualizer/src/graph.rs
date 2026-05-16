use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphConfig {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub selectors: Vec<MetricSelector>,
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub selectors: Vec<MetricSelector>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricSelector {
    #[serde(default)]
    pub metric_exact: Option<String>,
    #[serde(default)]
    pub metric_prefix: Option<String>,
    #[serde(default)]
    pub metric_type: Option<String>,
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub workload: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
}

impl GraphConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read graph config: {}", path.display()))?;
        let config: Self = serde_json::from_str(&file)
            .with_context(|| format!("failed to parse graph config json: {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let mut node_ids = std::collections::HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                bail!("graph node has empty id");
            }
            if !node_ids.insert(node.id.clone()) {
                bail!("duplicate graph node id: {}", node.id);
            }
        }

        let mut edge_ids = std::collections::HashSet::new();
        for edge in &self.edges {
            if !edge_ids.insert(edge.id.clone()) {
                bail!("duplicate graph edge id: {}", edge.id);
            }
            if !node_ids.contains(&edge.from) {
                bail!(
                    "edge {} references unknown from node {}",
                    edge.id,
                    edge.from
                );
            }
            if !node_ids.contains(&edge.to) {
                bail!("edge {} references unknown to node {}", edge.id, edge.to);
            }
        }

        Ok(())
    }
}

impl MetricSelector {
    pub fn matches(&self, metric: &crate::protocol::MetricRow) -> bool {
        if let Some(exact) = &self.metric_exact {
            if &metric.metric != exact {
                return false;
            }
        }
        if let Some(prefix) = &self.metric_prefix {
            if !metric.metric.starts_with(prefix) {
                return false;
            }
        }
        if let Some(metric_type) = &self.metric_type {
            if &metric.metric_type != metric_type {
                return false;
            }
        }
        if let Some(process) = &self.process {
            if metric.process.as_deref() != Some(process.as_str()) {
                return false;
            }
        }
        if let Some(run) = &self.run {
            if metric.run.as_deref() != Some(run.as_str()) {
                return false;
            }
        }
        if let Some(workload) = &self.workload {
            if !metric.workloads.iter().any(|w| w == workload) {
                return false;
            }
        }
        if let Some(variant) = &self.variant {
            if metric.variant.as_deref() != Some(variant.as_str()) {
                return false;
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_matches_metric() {
        let selector = MetricSelector {
            metric_prefix: Some("q.".into()),
            metric_type: Some("queue".into()),
            process: Some("client:0".into()),
            ..Default::default()
        };

        let row = crate::protocol::MetricRow {
            metric: "q.dispatch".into(),
            metric_type: "queue".into(),
            process: Some("client:0".into()),
            run: None,
            workloads: vec![],
            ts: Some(1.0),
            variant: None,
            enq: Some(1),
            drain: Some(1),
            depth: Some(0),
            count: None,
            p50: None,
            p99: None,
            max: None,
            unit: None,
            value: None,
            hit: None,
            miss: None,
            bytes: None,
        };

        assert!(selector.matches(&row));
    }
}
