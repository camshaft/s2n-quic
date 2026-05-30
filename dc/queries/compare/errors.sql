-- Error totals by label, showing which run had more failures.
CREATE OR REPLACE VIEW compare_errors AS
SELECT
    label,
    metric,
    variant,
    SUM(CAST(value AS BIGINT))  AS total
FROM runs
WHERE metric LIKE '!%'
  AND type IN ('nominal', 'scalar')
GROUP BY label, metric, variant
HAVING total > 0
ORDER BY label, total DESC, metric;
