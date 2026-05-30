-- TX and RX throughput per minute over the course of the run.
-- Useful for spotting ramp-up, saturation, or throughput drops.
CREATE OR REPLACE VIEW dashboard_throughput_ts AS
SELECT
    date_trunc('minute', to_timestamp(ts))                                         AS minute,
    ROUND(SUM(bytes) FILTER (WHERE metric = 'socket.tx.bytes') * 8.0 / 1e9, 3)   AS tx_gbps,
    ROUND(SUM(bytes) FILTER (WHERE metric = 'socket.rx.bytes') * 8.0 / 1e9, 3)   AS rx_gbps
FROM metrics
WHERE type = 'throughput'
  AND metric IN ('socket.tx.bytes', 'socket.rx.bytes')
GROUP BY 1
ORDER BY 1;
