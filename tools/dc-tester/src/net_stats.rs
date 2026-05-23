// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_os = "linux")]
pub fn spawn(counters: s2n_quic_dc::counter::Registry) {
    tokio::spawn(async {
        use std::collections::HashMap;
        use std::time::Duration;
        use tokio::time;

        let mut interval = time::interval(Duration::from_secs(1));
        let mut reporter = Reporter::new(counters);
        let mut prev = Snapshot::read();

        loop {
            interval.tick().await;
            let current = Snapshot::read();
            let mut delta = HashMap::new();
            for (key, value) in &current.values {
                let prev_value = prev.values.get(key).copied().unwrap_or_default();
                let diff = value.saturating_sub(prev_value);
                if diff != 0 {
                    delta.insert(key.as_str(), diff);
                }
            }
            reporter.record_delta(delta.into_iter());
            prev = current;
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub fn spawn(_counters: s2n_quic_dc::counter::Registry) {}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct Reporter {
    counters: std::collections::HashMap<String, s2n_quic_dc::counter::Counter>,
    registry: s2n_quic_dc::counter::Registry,
}

#[cfg(target_os = "linux")]
impl Reporter {
    fn new(registry: s2n_quic_dc::counter::Registry) -> Self {
        Self {
            counters: Default::default(),
            registry,
        }
    }

    fn counter(&mut self, key: &str) -> &s2n_quic_dc::counter::Counter {
        self.counters
            .entry(key.to_string())
            .or_insert_with(|| self.registry.register(key))
    }

    fn record_delta<'a>(&mut self, entries: impl Iterator<Item = (&'a str, u64)>) {
        for (key, value) in entries {
            self.counter(key).add(value);
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default, Clone)]
struct Snapshot {
    values: std::collections::HashMap<String, u64>,
}

#[cfg(target_os = "linux")]
impl Snapshot {
    fn read() -> Self {
        let mut values = std::collections::HashMap::new();
        collect_snmp_pairs(&mut values, "/proc/net/snmp", "os.netstat");
        collect_snmp_pairs(&mut values, "/proc/net/netstat", "os.netstat_ext");
        collect_udp_table_sums(&mut values, "/proc/net/udp", "os.udp");
        collect_udp_table_sums(&mut values, "/proc/net/udp6", "os.udp6");
        collect_sockstat(&mut values, "/proc/net/sockstat", "os.sockstat");
        collect_sockstat(&mut values, "/proc/net/sockstat6", "os.sockstat6");
        collect_softnet_stat(&mut values, "/proc/net/softnet_stat");
        collect_netdev(&mut values, "/proc/net/dev");
        Self { values }
    }
}

#[cfg(target_os = "linux")]
fn collect_snmp_pairs(out: &mut std::collections::HashMap<String, u64>, path: &str, prefix: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    parse_snmp_pairs_content(out, &content, prefix);
}

#[cfg(target_os = "linux")]
fn parse_snmp_pairs_content(
    out: &mut std::collections::HashMap<String, u64>,
    content: &str,
    prefix: &str,
) {
    let mut lines = content.lines();
    while let Some(header_line) = lines.next() {
        let Some(value_line) = lines.next() else {
            break;
        };
        let headers: Vec<&str> = header_line.split_whitespace().collect();
        let values: Vec<&str> = value_line.split_whitespace().collect();
        if headers.is_empty() || values.is_empty() || headers[0] != values[0] {
            continue;
        }

        let section = sanitize_metric_token(headers[0].trim_end_matches(':'));
        for (idx, header) in headers.iter().enumerate().skip(1) {
            let Some(value) = parse_numeric(values.get(idx).copied()) else {
                continue;
            };
            out.insert(
                format!("{prefix}.{section}.{}", sanitize_metric_token(header)),
                value,
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_udp_table_sums(
    out: &mut std::collections::HashMap<String, u64>,
    path: &str,
    prefix: &str,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut lines = content.lines();
    let Some(header_line) = lines.next() else {
        return;
    };
    let headers: Vec<&str> = header_line.split_whitespace().collect();
    if headers.len() < 2 {
        return;
    }
    let mut sums = vec![0u64; headers.len() - 1];
    for line in lines {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < headers.len() {
            continue;
        }
        for (idx, value) in cols.iter().enumerate().skip(1) {
            if let Some(value) = parse_numeric(Some(value)) {
                sums[idx - 1] = sums[idx - 1].saturating_add(value);
            }
        }
    }
    for (header, sum) in headers.iter().skip(1).zip(sums) {
        out.insert(format!("{prefix}.{}", sanitize_metric_token(header)), sum);
    }
}

#[cfg(target_os = "linux")]
fn collect_sockstat(out: &mut std::collections::HashMap<String, u64>, path: &str, prefix: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let mut cols = line.split_whitespace();
        let Some(section) = cols.next() else { continue };
        let section = sanitize_metric_token(section.trim_end_matches(':'));
        let kv: Vec<&str> = cols.collect();
        for pair in kv.chunks_exact(2) {
            if let Some(value) = parse_numeric(Some(pair[1])) {
                out.insert(
                    format!(
                        "{prefix}.{section}.{}",
                        sanitize_metric_token(pair[0].trim_end_matches(':'))
                    ),
                    value,
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_softnet_stat(out: &mut std::collections::HashMap<String, u64>, path: &str) {
    const FIELDS: [&str; 6] = [
        "processed",
        "dropped",
        "time_squeeze",
        "cpu_collision",
        "received_rps",
        "flow_limit_count",
    ];
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    parse_softnet_content(out, &content, &FIELDS);
}

#[cfg(target_os = "linux")]
fn parse_softnet_content(
    out: &mut std::collections::HashMap<String, u64>,
    content: &str,
    fields: &[&str],
) {
    let mut sums = vec![0u64; fields.len()];
    for line in content.lines() {
        for (idx, value) in line.split_whitespace().take(fields.len()).enumerate() {
            let Ok(value) = u64::from_str_radix(value, 16) else {
                continue;
            };
            sums[idx] = sums[idx].saturating_add(value);
        }
    }
    for (field, sum) in fields.iter().zip(sums) {
        out.insert(format!("os.qdisc.softnet.{field}"), sum);
    }
}

#[cfg(target_os = "linux")]
fn collect_netdev(out: &mut std::collections::HashMap<String, u64>, path: &str) {
    const RX_FIELDS: [&str; 8] = [
        "rx_bytes",
        "rx_packets",
        "rx_errs",
        "rx_drop",
        "rx_fifo",
        "rx_frame",
        "rx_compressed",
        "rx_multicast",
    ];
    const TX_FIELDS: [&str; 8] = [
        "tx_bytes",
        "tx_packets",
        "tx_errs",
        "tx_drop",
        "tx_fifo",
        "tx_colls",
        "tx_carrier",
        "tx_compressed",
    ];
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = sanitize_metric_token(iface.trim());
        let values: Vec<&str> = rest.split_whitespace().collect();
        if values.len() < 16 {
            continue;
        }
        for (field, value) in RX_FIELDS.iter().zip(values.iter().take(8)) {
            if let Some(value) = parse_numeric(Some(value)) {
                out.insert(format!("os.ethtool.netdev.{field}.{iface}"), value);
            }
        }
        for (field, value) in TX_FIELDS.iter().zip(values.iter().skip(8).take(8)) {
            if let Some(value) = parse_numeric(Some(value)) {
                out.insert(format!("os.ethtool.netdev.{field}.{iface}"), value);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn parse_numeric(value: Option<&str>) -> Option<u64> {
    let value = value?;
    value
        .parse::<u64>()
        .ok()
        .or_else(|| u64::from_str_radix(value, 16).ok())
}

#[cfg(target_os = "linux")]
fn sanitize_metric_token(token: &str) -> String {
    token
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => c.to_ascii_lowercase(),
            _ => '_',
        })
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parse_snmp_lines_extracts_udp_fields() {
        let content = "\
Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors
Udp: 10 0 5 20 3 2
";
        let mut out = std::collections::HashMap::new();
        parse_snmp_pairs_content(&mut out, content, "os.netstat");
        assert_eq!(out.get("os.netstat.udp.inerrors"), Some(&5));
        assert_eq!(out.get("os.netstat.udp.rcvbuferrors"), Some(&3));
    }

    #[test]
    fn softnet_parses_hex_columns() {
        let mut out = std::collections::HashMap::new();
        parse_softnet_content(
            &mut out,
            "00000001 00000002 00000003 00000004 00000005 00000006\n00000001 00000001 00000001 00000001 00000001 00000001\n",
            &[
                "processed",
                "dropped",
                "time_squeeze",
                "cpu_collision",
                "received_rps",
                "flow_limit_count",
            ],
        );
        assert_eq!(out.get("os.qdisc.softnet.processed"), Some(&2));
        assert_eq!(out.get("os.qdisc.softnet.dropped"), Some(&3));
    }
}
