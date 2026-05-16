use crate::protocol::{Alert, MetricRow, MetricSeries, Snapshot, TimePoint};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::PathBuf,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub history_window: Duration,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            history_window: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    config: StoreConfig,
    entries: HashMap<String, Entry>,
    runs: BTreeSet<String>,
    processes: BTreeSet<String>,
    workloads: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    latest: MetricRow,
    history: VecDeque<TimePoint>,
}

impl Store {
    pub fn new(config: StoreConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            runs: BTreeSet::new(),
            processes: BTreeSet::new(),
            workloads: BTreeSet::new(),
        }
    }

    pub fn ingest(&mut self, metric: MetricRow) -> MetricSeries {
        if let Some(run) = &metric.run {
            self.runs.insert(run.clone());
        }
        if let Some(process) = &metric.process {
            self.processes.insert(process.clone());
        }
        for workload in &metric.workloads {
            self.workloads.insert(workload.clone());
        }

        let key = metric_key(&metric);
        let now = metric.ts.unwrap_or_else(now_ts);

        let point = TimePoint {
            ts: now,
            depth: metric.depth,
            enq: metric.enq,
            drain: metric.drain,
            p99: metric.p99,
        };

        let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
            latest: metric.clone(),
            history: VecDeque::new(),
        });
        entry.latest = metric;
        entry.history.push_back(point);

        let cutoff = now - self.config.history_window.as_secs_f64();
        while let Some(front) = entry.history.front() {
            if front.ts < cutoff {
                let _ = entry.history.pop_front();
            } else {
                break;
            }
        }

        MetricSeries {
            key,
            latest: entry.latest.clone(),
            history: entry.history.iter().cloned().collect(),
            alerts: derive_alerts(&entry.latest, &entry.history),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut metrics = Vec::with_capacity(self.entries.len());
        for (key, entry) in &self.entries {
            metrics.push(MetricSeries {
                key: key.clone(),
                latest: entry.latest.clone(),
                history: entry.history.iter().cloned().collect(),
                alerts: derive_alerts(&entry.latest, &entry.history),
            });
        }

        Snapshot {
            metrics,
            available_runs: self.runs.iter().cloned().collect(),
            available_processes: self.processes.iter().cloned().collect(),
            available_workloads: self.workloads.iter().cloned().collect(),
        }
    }
}

pub async fn tail_metrics_file(
    path: PathBuf,
    mut on_metric: impl FnMut(MetricRow) + Send + 'static,
) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to open metrics file: {}", path.display()))?;

    let mut reader = BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(0)).await?;

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<MetricRow>(line) {
            Ok(metric) => on_metric(metric),
            Err(error) => {
                tracing::warn!(?error, line, "failed to parse metrics json row");
            }
        }
    }
}

fn metric_key(metric: &MetricRow) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        metric.metric_type,
        metric.metric,
        metric.variant.clone().unwrap_or_default(),
        metric.process.clone().unwrap_or_default(),
        metric.run.clone().unwrap_or_default()
    )
}

fn derive_alerts(metric: &MetricRow, history: &VecDeque<TimePoint>) -> Vec<Alert> {
    let mut alerts = Vec::new();

    if metric.metric_type == "queue" {
        if let (Some(latest_depth), Some(first_depth)) =
            (metric.depth, history.front().and_then(|point| point.depth))
        {
            if latest_depth > first_depth + 10 {
                alerts.push(Alert {
                    kind: "queue_depth_growth".into(),
                    severity: "warning".into(),
                    message: format!("queue depth increased from {first_depth} to {latest_depth}"),
                });
            }
        }

        if let (Some(enq), Some(drain), Some(depth)) = (metric.enq, metric.drain, metric.depth) {
            if enq > drain.saturating_mul(12) / 10 && depth > 0 {
                alerts.push(Alert {
                    kind: "drain_enq_imbalance".into(),
                    severity: "warning".into(),
                    message: format!("enqueue ({enq}) outpacing drain ({drain})"),
                });
            }
        }
    }

    if metric.metric_type == "histogram"
        && metric.metric.starts_with("task")
        && history.len() >= 4
        && metric.p99.is_some()
    {
        let latest = metric.p99.unwrap_or_default();
        let mut baseline_values: Vec<f64> = history.iter().filter_map(|point| point.p99).collect();
        baseline_values.sort_by(|a, b| a.total_cmp(b));
        let baseline = baseline_values[baseline_values.len() / 2];
        if baseline > 0.0 && latest > baseline * 1.5 {
            alerts.push(Alert {
                kind: "task_latency_spike".into(),
                severity: "critical".into(),
                message: format!("task p99 {latest:.3} is above baseline {baseline:.3}"),
            });
        }
    }

    alerts
}

pub fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn store_generates_alerts_for_queue_growth() {
        let mut store = Store::new(StoreConfig::default());
        for depth in [1, 5, 9, 20] {
            let row = MetricRow {
                metric: "q.dispatch".into(),
                metric_type: "queue".into(),
                variant: None,
                process: Some("client:0".into()),
                run: Some("run1".into()),
                workloads: vec!["default".into()],
                ts: Some(depth as f64),
                enq: Some(100),
                drain: Some(60),
                depth: Some(depth),
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
            let _ = store.ingest(row);
        }

        let snapshot = store.snapshot();
        let queue = snapshot
            .metrics
            .iter()
            .find(|series| series.latest.metric == "q.dispatch")
            .expect("queue series");
        assert!(
            queue
                .alerts
                .iter()
                .any(|alert| alert.kind == "queue_depth_growth")
        );
    }

    #[tokio::test]
    async fn tailer_reads_appended_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("metrics.jsonl");
        let mut file = tokio::fs::File::create(&path).await.expect("create file");
        file.write_all(b"{\"metric\":\"q.dispatch\",\"type\":\"queue\",\"enq\":1,\"drain\":1,\"depth\":0,\"ts\":1.0}\n")
            .await
            .expect("write line");
        file.flush().await.expect("flush");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let path_clone = path.clone();
        let handle = tokio::spawn(async move {
            let _ = tail_metrics_file(path_clone, move |metric| {
                let _ = tx.send(metric);
            })
            .await;
        });

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("metric");
        assert_eq!(received.metric, "q.dispatch");

        handle.abort();
    }
}
