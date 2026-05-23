// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Linux OS counter ingestion for endpoint metrics.
//!
//! This module snapshots `/proc` network stats and records per-interval
//! deltas (for monotonic counters) and current values (for gauges) into the
//! shared `counter::Registry`. It is driven by the reporter background thread
//! via [`ReporterConfig::os_stats`][crate::counter::ReporterConfig]; the
//! [`Collector`] is created automatically when that flag is set.

#[cfg(target_os = "linux")]
use std::collections::HashMap;

/// A Linux OS metrics collector.
///
/// On each call to [`record_delta`][Collector::record_delta] it reads the
/// current `/proc` snapshots and:
///
/// - for **monotonic** sources (`/proc/net/snmp`, `/proc/net/netstat`,
///   `/proc/net/softnet_stat`, `/proc/net/dev`, and the UDP drops column from
///   `/proc/net/udp`/`udp6`): computes the per-interval delta and increments
///   the corresponding `Counter`.
/// - for **gauge** sources (`/proc/net/sockstat`, `/proc/net/sockstat6`):
///   sets the corresponding `Gauge` to the current absolute value.
#[cfg(target_os = "linux")]
pub struct Collector {
    counters: HashMap<String, crate::counter::Counter>,
    gauges: HashMap<String, crate::counter::Gauge>,
    registry: crate::counter::Registry,
    prev_monotonic: HashMap<String, u64>,
}

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub struct Collector;

#[cfg(target_os = "linux")]
impl Collector {
    pub fn new(registry: crate::counter::Registry) -> Self {
        Self {
            counters: Default::default(),
            gauges: Default::default(),
            prev_monotonic: read_monotonic_snapshot(),
            registry,
        }
    }

    fn counter(&mut self, key: &str) -> &crate::counter::Counter {
        let registry = &self.registry;
        self.counters
            .entry(key.to_string())
            .or_insert_with(|| registry.register(key).with_description(counter_description(key)))
    }

    fn gauge(&mut self, key: &str) -> &crate::counter::Gauge {
        let registry = &self.registry;
        self.gauges
            .entry(key.to_string())
            .or_insert_with(|| registry.register_gauge(key).with_description(gauge_description(key)))
    }

    /// Reads fresh snapshots and updates all registered metrics.
    ///
    /// Monotonic sources are delta-encoded into `Counter`s. Gauge sources are
    /// set to their current absolute value.
    pub fn record_delta(&mut self) {
        // ── Monotonic counters ───────────────────────────────────────────────
        let current = read_monotonic_snapshot();
        let deltas: Vec<(String, u64)> = current
            .iter()
            .filter_map(|(key, &value)| {
                let prev = self.prev_monotonic.get(key).copied().unwrap_or_default();
                let diff = value.saturating_sub(prev);
                (diff > 0).then_some((key.clone(), diff))
            })
            .collect();
        self.prev_monotonic = current;
        for (key, diff) in &deltas {
            self.counter(key).add(*diff);
        }

        // ── Gauges ───────────────────────────────────────────────────────────
        let gauge_values = read_gauge_snapshot();
        for (key, value) in &gauge_values {
            self.gauge(key).set(*value as i64);
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Collector {
    pub fn new(_registry: crate::counter::Registry) -> Self {
        Self
    }

    pub fn record_delta(&mut self) {}
}

// ── Snapshot functions ────────────────────────────────────────────────────────

/// Reads all monotonically-increasing OS counters.
#[cfg(target_os = "linux")]
fn read_monotonic_snapshot() -> HashMap<String, u64> {
    let mut values = HashMap::new();
    collect_snmp_pairs(&mut values, "/proc/net/snmp", "os.netstat");
    collect_snmp_pairs(&mut values, "/proc/net/netstat", "os.netstat_ext");
    collect_udp_drops(&mut values, "/proc/net/udp", "os.udp");
    collect_udp_drops(&mut values, "/proc/net/udp6", "os.udp6");
    collect_softnet_stat(&mut values, "/proc/net/softnet_stat");
    collect_netdev(&mut values, "/proc/net/dev");
    values
}

/// Reads all gauge-like OS statistics (current values, not cumulative).
#[cfg(target_os = "linux")]
fn read_gauge_snapshot() -> HashMap<String, u64> {
    let mut values = HashMap::new();
    collect_sockstat(&mut values, "/proc/net/sockstat", "os.sockstat");
    collect_sockstat(&mut values, "/proc/net/sockstat6", "os.sockstat6");
    values
}

// ── Description helpers ───────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn counter_description(key: &str) -> &'static str {
    if key.starts_with("os.netstat_ext.") {
        "Extended kernel network statistics from /proc/net/netstat"
    } else if key.starts_with("os.netstat.") {
        "Kernel network statistics from /proc/net/snmp"
    } else if key.starts_with("os.udp6.") {
        "IPv6 UDP socket drop count from /proc/net/udp6"
    } else if key.starts_with("os.udp.") {
        "UDP socket drop count from /proc/net/udp"
    } else if key.starts_with("os.qdisc.softnet.") {
        "Software network queue statistics from /proc/net/softnet_stat"
    } else if key.starts_with("os.ethtool.netdev.") {
        "Network interface statistics from /proc/net/dev"
    } else {
        ""
    }
}

#[cfg(target_os = "linux")]
fn gauge_description(key: &str) -> &'static str {
    if key.starts_with("os.sockstat6.") {
        "IPv6 socket allocation statistics from /proc/net/sockstat6"
    } else if key.starts_with("os.sockstat.") {
        "Socket allocation statistics from /proc/net/sockstat"
    } else {
        ""
    }
}

// ── Parsers ───────────────────────────────────────────────────────────────────

/// Parses SNMP-style paired header/value lines from `/proc/net/snmp` and
/// `/proc/net/netstat`.
///
/// Each logical entry spans two consecutive lines that share the same section
/// prefix (e.g. `"Udp:"` / `"Udp:"`): the first lists field names and the
/// second lists the corresponding integer values.
#[cfg(target_os = "linux")]
fn collect_snmp_pairs(out: &mut HashMap<String, u64>, path: &str, prefix: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    parse_snmp_pairs_content(out, &content, prefix);
}

#[cfg(target_os = "linux")]
fn parse_snmp_pairs_content(out: &mut HashMap<String, u64>, content: &str, prefix: &str) {
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

/// Collects only the `drops` counter from `/proc/net/udp` or `/proc/net/udp6`.
///
/// The table contains per-socket rows. Only the `drops` column represents a
/// meaningful monotonic counter; all other columns are per-socket identifiers
/// or state fields that are not suitable for delta encoding.
///
/// The header lists `drops` as its last field. In the data rows, `tx_queue` and
/// `rx_queue` are merged into a single `hex:hex` token, as are `tr` and
/// `tm->when`, so the data row has two fewer columns than the header — but
/// `drops` remains the last column in both.
#[cfg(target_os = "linux")]
fn collect_udp_drops(out: &mut HashMap<String, u64>, path: &str, prefix: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    parse_udp_drops_content(out, &content, prefix);
}

#[cfg(target_os = "linux")]
fn parse_udp_drops_content(out: &mut HashMap<String, u64>, content: &str, prefix: &str) {
    let mut lines = content.lines();
    let Some(header_line) = lines.next() else {
        return;
    };
    // Confirm the header mentions "drops" (last field).
    let has_drops = header_line
        .split_whitespace()
        .any(|h| h.eq_ignore_ascii_case("drops"));
    if !has_drops {
        return;
    }
    // In data rows, `drops` is always the last whitespace-separated token.
    let mut total_drops: u64 = 0;
    for line in lines {
        if let Some(last) = line.split_whitespace().last() {
            if let Some(v) = parse_numeric(Some(last)) {
                total_drops = total_drops.saturating_add(v);
            }
        }
    }
    out.insert(format!("{prefix}.drops"), total_drops);
}

/// Parses `/proc/net/sockstat` and `/proc/net/sockstat6`.
///
/// Values are current socket allocation counts (gauges, not monotonic counters).
#[cfg(target_os = "linux")]
fn collect_sockstat(out: &mut HashMap<String, u64>, path: &str, prefix: &str) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    parse_sockstat_content(out, &content, prefix);
}

#[cfg(target_os = "linux")]
fn parse_sockstat_content(out: &mut HashMap<String, u64>, content: &str, prefix: &str) {
    for line in content.lines() {
        let mut cols = line.split_whitespace();
        let Some(section) = cols.next() else {
            continue;
        };
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

/// Aggregates per-CPU rows from `/proc/net/softnet_stat`.
///
/// Each row corresponds to one CPU and contains space-separated hexadecimal
/// counters. The values are summed across all CPUs.
#[cfg(target_os = "linux")]
fn collect_softnet_stat(out: &mut HashMap<String, u64>, path: &str) {
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
fn parse_softnet_content(out: &mut HashMap<String, u64>, content: &str, fields: &[&str]) {
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

/// Reads per-interface RX and TX statistics from `/proc/net/dev`.
#[cfg(target_os = "linux")]
fn collect_netdev(out: &mut HashMap<String, u64>, path: &str) {
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
    parse_netdev_content(out, &content, &RX_FIELDS, &TX_FIELDS);
}

#[cfg(target_os = "linux")]
fn parse_netdev_content(
    out: &mut HashMap<String, u64>,
    content: &str,
    rx_fields: &[&str],
    tx_fields: &[&str],
) {
    for line in content.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = sanitize_metric_token(iface.trim());
        let values: Vec<&str> = rest.split_whitespace().collect();
        if values.len() < 16 {
            continue;
        }
        for (field, value) in rx_fields.iter().zip(values.iter().take(8)) {
            if let Some(value) = parse_numeric(Some(value)) {
                out.insert(format!("os.ethtool.netdev.{field}.{iface}"), value);
            }
        }
        for (field, value) in tx_fields.iter().zip(values.iter().skip(8).take(8)) {
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

/// Normalizes a raw token from `/proc` into a stable metric label segment.
///
/// Only ASCII alphanumeric characters are preserved (lowercased); all other
/// characters are replaced with `_`.
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

    // ── SNMP / netstat ────────────────────────────────────────────────────────

    #[test]
    fn snmp_extracts_udp_fields() {
        let content = "\
Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors
Udp: 10 0 5 20 3 2
";
        let mut out = HashMap::new();
        parse_snmp_pairs_content(&mut out, content, "os.netstat");
        assert_eq!(out.get("os.netstat.udp.inerrors"), Some(&5));
        assert_eq!(out.get("os.netstat.udp.rcvbuferrors"), Some(&3));
        assert_eq!(out.get("os.netstat.udp.indatagrams"), Some(&10));
    }

    #[test]
    fn snmp_skips_mismatched_section_labels() {
        // Header section prefix doesn't match value line prefix → skip.
        let content = "\
Tcp: RtoAlgorithm RtoMin
Udp: 1 2
";
        let mut out = HashMap::new();
        parse_snmp_pairs_content(&mut out, content, "os.netstat");
        assert!(out.is_empty());
    }

    // ── UDP drops ─────────────────────────────────────────────────────────────

    #[test]
    fn udp_drops_only_collects_drops_column() {
        // Real /proc/net/udp format: header has 15 tokens but data rows only have 13
        // (tx_queue:rx_queue and tr:tm->when are merged in data rows). drops is last.
        let content = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
   0: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000   101        0 12345 2 0000000000000000 5
   1: 0F02000A:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000   101        0 67890 2 0000000000000000 3
";
        let mut out = HashMap::new();
        parse_udp_drops_content(&mut out, content, "os.udp");
        // Only drops should be present, summed across sockets
        assert_eq!(out.get("os.udp.drops"), Some(&8));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn udp_drops_missing_drops_header_yields_nothing() {
        let content = "\
  sl  local_address rem_address   st
   0: 00000000:0035 00000000:0000 07
";
        let mut out = HashMap::new();
        parse_udp_drops_content(&mut out, content, "os.udp");
        assert!(out.is_empty());
    }

    // ── Sockstat ─────────────────────────────────────────────────────────────

    #[test]
    fn sockstat_parses_key_value_pairs() {
        let content = "\
sockets: used 128
TCP: inuse 21 orphan 0 tw 0 alloc 21 mem 3
UDP: inuse 12 mem 0
UDPLITE: inuse 0
RAW: inuse 0
FRAG: inuse 0 memory 0
";
        let mut out = HashMap::new();
        parse_sockstat_content(&mut out, content, "os.sockstat");
        assert_eq!(out.get("os.sockstat.sockets.used"), Some(&128));
        assert_eq!(out.get("os.sockstat.tcp.inuse"), Some(&21));
        assert_eq!(out.get("os.sockstat.udp.inuse"), Some(&12));
        assert_eq!(out.get("os.sockstat.tcp.mem"), Some(&3));
        assert_eq!(out.get("os.sockstat.frag.memory"), Some(&0));
    }

    #[test]
    fn sockstat_handles_empty_content() {
        let mut out = HashMap::new();
        parse_sockstat_content(&mut out, "", "os.sockstat");
        assert!(out.is_empty());
    }

    // ── Softnet ───────────────────────────────────────────────────────────────

    #[test]
    fn softnet_sums_hex_columns_across_cpus() {
        let mut out = HashMap::new();
        parse_softnet_content(
            &mut out,
            "00000001 00000002 00000003 00000004 00000005 00000006\n\
             00000001 00000001 00000001 00000001 00000001 00000001\n",
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
        assert_eq!(out.get("os.qdisc.softnet.time_squeeze"), Some(&4));
    }

    // ── Netdev ────────────────────────────────────────────────────────────────

    #[test]
    fn netdev_extracts_rx_tx_per_interface() {
        // /proc/net/dev format: 2 header lines, then "iface: <16 values>"
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  123456   1000    0    0    0     0          0         0   654321    2000    0    0    0     0       0          0
   eth0: 999999   9999    1    2    3     4          5         6   111111    1111    7    8    9    10      11         12
";
        let mut out = HashMap::new();
        let rx_fields = [
            "rx_bytes",
            "rx_packets",
            "rx_errs",
            "rx_drop",
            "rx_fifo",
            "rx_frame",
            "rx_compressed",
            "rx_multicast",
        ];
        let tx_fields = [
            "tx_bytes",
            "tx_packets",
            "tx_errs",
            "tx_drop",
            "tx_fifo",
            "tx_colls",
            "tx_carrier",
            "tx_compressed",
        ];
        parse_netdev_content(&mut out, content, &rx_fields, &tx_fields);
        assert_eq!(out.get("os.ethtool.netdev.rx_bytes.lo"), Some(&123456));
        assert_eq!(out.get("os.ethtool.netdev.tx_bytes.lo"), Some(&654321));
        assert_eq!(out.get("os.ethtool.netdev.rx_bytes.eth0"), Some(&999999));
        assert_eq!(out.get("os.ethtool.netdev.rx_errs.eth0"), Some(&1));
        assert_eq!(out.get("os.ethtool.netdev.tx_drop.eth0"), Some(&8));
    }

    #[test]
    fn netdev_skips_lines_with_too_few_columns() {
        let content = "\
Inter-|   Receive
 face |bytes
    lo: 1 2 3
";
        let mut out = HashMap::new();
        parse_netdev_content(&mut out, content, &[], &[]);
        assert!(out.is_empty());
    }

    // ── sanitize_metric_token ─────────────────────────────────────────────────

    #[test]
    fn sanitize_lowercases_and_replaces_special_chars() {
        assert_eq!(sanitize_metric_token("InErrors"), "inerrors");
        assert_eq!(sanitize_metric_token("rx-bytes"), "rx_bytes");
        assert_eq!(sanitize_metric_token("TCP:"), "tcp_");
        assert_eq!(sanitize_metric_token("tm->when"), "tm__when");
        assert_eq!(sanitize_metric_token("abc123"), "abc123");
    }
}
