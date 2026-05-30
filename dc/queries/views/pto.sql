-- PTO (Probe Timeout) probe counts and backoff distribution.
-- tx.probe.no_response: probes sent with no ACK received
-- tx.probe.frame: probe frames transmitted
-- tx.probe.backoff: distribution of backoff multipliers applied
-- High backoff values (variant > 4) suggest chronic path or receiver issues.
CREATE OR REPLACE VIEW pto AS
SELECT
    metric,
    variant,
    SUM(CAST(value AS BIGINT))  AS total
FROM metrics
WHERE (metric LIKE 'tx.probe.%' OR metric LIKE '!tx.probe.%')
  AND type = 'nominal'
GROUP BY metric, variant

UNION ALL

SELECT
    metric,
    NULL                        AS variant,
    SUM(count)                  AS total
FROM metrics
WHERE (metric LIKE 'tx.probe.%' OR metric LIKE '!tx.probe.%')
  AND type = 'histogram'
GROUP BY metric

ORDER BY metric, variant;
