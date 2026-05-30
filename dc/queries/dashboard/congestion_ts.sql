-- Congestion window (cwnd) and RTT trends per minute.
-- A shrinking cwnd alongside rising RTT typically indicates congestion.
CREATE OR REPLACE VIEW dashboard_congestion_ts AS
SELECT
    date_trunc('minute', to_timestamp(ts))  AS minute,
    MAX(p50) FILTER (WHERE metric = 'send.cwnd')        AS cwnd_p50_bytes,
    MAX(p99) FILTER (WHERE metric = 'send.cwnd')        AS cwnd_p99_bytes,
    MAX(p50) FILTER (WHERE metric = 'tx.rtt')           AS rtt_p50_us,
    MAX(p99) FILTER (WHERE metric = 'tx.rtt')           AS rtt_p99_us
FROM metrics
WHERE type = 'histogram'
  AND metric IN ('send.cwnd', 'tx.rtt')
GROUP BY 1
ORDER BY 1;
