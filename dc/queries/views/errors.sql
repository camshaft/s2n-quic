-- All error counters (metrics whose name starts with '!').
-- Includes packet loss, routing asymmetry, decode failures, and more.
-- Any non-zero total warrants investigation.
CREATE OR REPLACE VIEW errors AS
SELECT
    metric,
    variant,
    SUM(CAST(value AS BIGINT))  AS total
FROM metrics
WHERE metric LIKE '!%'
  AND type IN ('nominal', 'scalar')
GROUP BY metric, variant
HAVING total > 0

UNION ALL

SELECT
    metric,
    NULL                        AS variant,
    SUM(count)                  AS total
FROM metrics
WHERE metric LIKE '!%'
  AND type = 'histogram'
GROUP BY metric
HAVING total > 0

ORDER BY total DESC, metric;
