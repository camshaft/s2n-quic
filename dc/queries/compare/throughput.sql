-- TX/RX throughput comparison across runs.
-- Each row is one (label, minute) pair so runs can be plotted side-by-side.
CREATE OR REPLACE VIEW compare_throughput AS
SELECT
    label,
    date_trunc('minute', to_timestamp(ts))                                         AS minute,
    ROUND(SUM(bytes) FILTER (WHERE metric = 'socket.tx.bytes') * 8.0 / 1e9, 3)   AS tx_gbps,
    ROUND(SUM(bytes) FILTER (WHERE metric = 'socket.rx.bytes') * 8.0 / 1e9, 3)   AS rx_gbps
FROM runs
WHERE type = 'throughput'
  AND metric IN ('socket.tx.bytes', 'socket.rx.bytes')
GROUP BY label, minute
ORDER BY label, minute;
