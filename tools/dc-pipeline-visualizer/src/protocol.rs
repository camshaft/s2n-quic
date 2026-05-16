use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRow {
    pub metric: String,
    #[serde(rename = "type")]
    pub metric_type: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub process: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub workloads: Vec<String>,
    #[serde(default)]
    pub ts: Option<f64>,

    #[serde(default)]
    pub enq: Option<u64>,
    #[serde(default)]
    pub drain: Option<u64>,
    #[serde(default)]
    pub depth: Option<i64>,

    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub p50: Option<f64>,
    #[serde(default)]
    pub p99: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,

    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub hit: Option<u64>,
    #[serde(default)]
    pub miss: Option<u64>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub kind: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimePoint {
    pub ts: f64,
    #[serde(default)]
    pub depth: Option<i64>,
    #[serde(default)]
    pub enq: Option<u64>,
    #[serde(default)]
    pub drain: Option<u64>,
    #[serde(default)]
    pub p99: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSeries {
    pub key: String,
    pub latest: MetricRow,
    pub history: Vec<TimePoint>,
    pub alerts: Vec<Alert>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub metrics: Vec<MetricSeries>,
    pub available_runs: Vec<String>,
    pub available_processes: Vec<String>,
    pub available_workloads: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Bootstrap {
        graph: crate::graph::GraphConfig,
        snapshot: Snapshot,
        now_ts: f64,
    },
    Metrics {
        updates: Vec<MetricSeries>,
    },
    Health {
        status: String,
        message: String,
    },
    State {
        state: String,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_contract_serializes() {
        let msg = ServerMessage::Bootstrap {
            graph: crate::graph::GraphConfig::default(),
            snapshot: Snapshot {
                metrics: Vec::new(),
                available_runs: Vec::new(),
                available_processes: Vec::new(),
                available_workloads: Vec::new(),
            },
            now_ts: 42.0,
        };

        let value = serde_json::to_value(msg).expect("serialize");
        assert_eq!(value["type"], "bootstrap");
        assert!(value.get("graph").is_some());
        assert!(value.get("snapshot").is_some());
    }

    #[test]
    fn metrics_contract_serializes() {
        let msg = ServerMessage::Metrics { updates: vec![] };
        let value = serde_json::to_value(msg).expect("serialize");
        assert_eq!(value["type"], "metrics");
        assert!(value.get("updates").is_some());
    }
}
