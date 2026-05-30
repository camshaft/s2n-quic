-- In-flight send state: congestion window (cwnd) and send-side queue lengths.
-- send.cwnd is a histogram (bytes); send.inflight.* are nominal counters.
-- A cwnd near zero or a growing inflight count may indicate congestion.
CREATE OR REPLACE VIEW inflight AS
SELECT
    metric,
    variant,
    SUM(count)              AS observations,
    MAX(p50)                AS cwnd_p50_bytes,
    MAX(p99)                AS cwnd_p99_bytes,
    MAX(max)                AS cwnd_max_bytes
FROM metrics
WHERE type = 'histogram'
  AND metric = 'send.cwnd'
GROUP BY metric, variant

UNION ALL

SELECT
    metric,
    variant,
    COUNT(*)                AS observations,
    NULL                    AS cwnd_p50_bytes,
    NULL                    AS cwnd_p99_bytes,
    CAST(MAX(value) AS UBIGINT) AS cwnd_max_bytes
FROM metrics
WHERE metric LIKE 'send.inflight%'
   OR metric = 'send.context.count'
GROUP BY metric, variant

ORDER BY metric, variant;
