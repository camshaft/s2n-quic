-- dc storage-IO correlation views (Tier-2 domain overlay for the per-op flight recorder).
--
-- Registered with `backbeat::register_views!` from `fs/trace.rs`, so every `.bb` dump carries this
-- DDL and `backbeat convert` appends it after the generated Tier-1 views. Run the convert sidecar
-- against the Parquet (`duckdb out.parquet -init out.views.sql`) and these macros are ready.
--
-- Keys (see `fs/trace.rs`):
--   * op key            = op_seq                       — a single operation, true identity.
--   * coarse fallback   = (device_index, fd, offset)   — for rows with no op_seq (NULL): the
--                         pre-admission lifecycles (Rejected, pre-acquire CancelledBeforeSubmit) and
--                         the one-shot credit events. The tuple is ambiguous across offset reuse, so
--                         it is a context view, not a join key.
-- op_seq is SQL NULL on those rows via the `sentinel = u64::MAX` declaration; ring_id is the
-- LocalRingId, with u32::MAX (UNSET) before the Dispatched lifecycle.
--
-- WHICH PROCESS a row came from: every row carries `instance_id` (a distinct per-process id stamped
-- by backbeat), separating ends in a merged dump. All `fs::io_op` events nest under the single event
-- type, so `lifecycle`/`kind` live in its struct column, pulled out below.
--
-- These build on the Tier-1 base `events` view and its dense columns: `seq` (global order within the
-- converted file), `ts_nanos`, `event` (event-type name), `event_id`, `instance_id`, plus the
-- promoted key columns (op_seq, user_data, fd, device_index, ring_id). Rows are ordered by
-- (ts_nanos, seq) so a deterministic simulated clock with ties still yields a stable total order.

-- One operation's whole lifecycle, time-ordered — the payoff of op_seq. submit → credit → dispatch →
-- backend → completion as a single, unambiguous timeline.
CREATE OR REPLACE MACRO op_timeline(seq_id) AS TABLE
  SELECT seq, ts_nanos, instance_id, op_seq, device_index, fd, ring_id,
         "s2n_quic_dc::fs::io_op::IoOpEvent".kind       AS kind,
         "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle  AS lifecycle,
         "s2n_quic_dc::fs::io_op::IoOpEvent".offset      AS offset,
         "s2n_quic_dc::fs::io_op::IoOpEvent".len         AS len,
         "s2n_quic_dc::fs::io_op::IoOpEvent".bytes_done  AS bytes_done,
         "s2n_quic_dc::fs::io_op::IoOpEvent".cost        AS cost,
         "s2n_quic_dc::fs::io_op::IoOpEvent".sojourn_us  AS sojourn_us,
         "s2n_quic_dc::fs::io_op::IoOpEvent".err_kind     AS err_kind
    FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
   WHERE op_seq = seq_id
   ORDER BY ts_nanos, seq;

-- Every op of one multi-op submitter (a materialize run), time-ordered. The stream-level counterpart
-- of op_timeline: group a whole streamed read by its shared stream_id, then read each op's lifecycle.
-- One-off submits carry a NULL stream_id and never appear here (trace those by op_timeline).
CREATE OR REPLACE MACRO stream_ops(sid) AS TABLE
  SELECT seq, ts_nanos, instance_id, stream_id, op_seq, user_data, device_index, fd, ring_id,
         "s2n_quic_dc::fs::io_op::IoOpEvent".kind       AS kind,
         "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle  AS lifecycle,
         "s2n_quic_dc::fs::io_op::IoOpEvent".offset      AS offset,
         "s2n_quic_dc::fs::io_op::IoOpEvent".bytes_done  AS bytes_done,
         "s2n_quic_dc::fs::io_op::IoOpEvent".sojourn_us  AS sojourn_us,
         "s2n_quic_dc::fs::io_op::IoOpEvent".err_kind     AS err_kind
    FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
   WHERE stream_id = sid
   ORDER BY ts_nanos, seq;

-- All ops touching one device (by its dense registration index), time-ordered. The per-device load
-- and error picture.
CREATE OR REPLACE MACRO device_ops(idx) AS TABLE
  SELECT seq, ts_nanos, instance_id, op_seq, fd, ring_id,
         "s2n_quic_dc::fs::io_op::IoOpEvent".kind       AS kind,
         "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle  AS lifecycle,
         "s2n_quic_dc::fs::io_op::IoOpEvent".offset      AS offset,
         "s2n_quic_dc::fs::io_op::IoOpEvent".bytes_done  AS bytes_done,
         "s2n_quic_dc::fs::io_op::IoOpEvent".sojourn_us  AS sojourn_us
    FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
   WHERE device_index = idx
   ORDER BY ts_nanos, seq;

-- Coarse fallback for rows that have no op_seq (Rejected / pre-acquire CancelledBeforeSubmit), or to
-- see everything that ever touched one disk offset. Ambiguous across offset reuse by construction —
-- a context view, not a single-op timeline (use op_timeline for that).
CREATE OR REPLACE MACRO offset_timeline(idx, f, off) AS TABLE
  SELECT seq, ts_nanos, instance_id, op_seq, ring_id,
         "s2n_quic_dc::fs::io_op::IoOpEvent".kind       AS kind,
         "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle  AS lifecycle,
         "s2n_quic_dc::fs::io_op::IoOpEvent".err_kind    AS err_kind
    FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
   WHERE device_index = idx AND fd = f
     AND "s2n_quic_dc::fs::io_op::IoOpEvent".offset = off
   ORDER BY ts_nanos, seq;

-- The IO-wedge smoking gun: ops that were Submitted/Dispatched but never reached a terminal lifecycle
-- (Completed / Failed / Orphaned / CancelledSkipped). A non-empty result for a healthy run is a stuck
-- op — a lost wakeup, a hung backend, or a never-draining lane.
CREATE OR REPLACE MACRO stuck_ops() AS TABLE
  WITH lc AS (
    SELECT op_seq,
           "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle AS lifecycle
      FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
     WHERE op_seq IS NOT NULL
  )
  SELECT op_seq
    FROM lc
   GROUP BY op_seq
  HAVING bool_or(lifecycle IN ('Submitted', 'Dispatched'))
     AND NOT bool_or(lifecycle IN ('Completed', 'Failed', 'Orphaned', 'CancelledSkipped'));

-- Credit starvation: per (device_index, kind, cost), the count of CreditPark vs CreditGrant rows.
-- The acquire primitive predates op_seq on the one-shot path, so a park cannot be joined to its own
-- grant; instead this aggregates by the coarse credit tuple. `parked > granted` means parks
-- outnumber grants for that tuple — acquires that have not (yet) been satisfied, the starvation
-- signal (a persistently undersized or wedged pool). `outstanding` is that difference.
CREATE OR REPLACE MACRO credit_waits() AS TABLE
  WITH c AS (
    SELECT device_index,
           "s2n_quic_dc::fs::io_op::IoOpEvent".kind      AS kind,
           "s2n_quic_dc::fs::io_op::IoOpEvent".cost      AS cost,
           "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle AS lifecycle
      FROM "s2n_quic_dc::fs::io_op::IoOpEvent"
     WHERE "s2n_quic_dc::fs::io_op::IoOpEvent".lifecycle IN ('CreditPark', 'CreditGrant')
  )
  SELECT device_index, kind, cost,
         count(*) FILTER (WHERE lifecycle = 'CreditPark')  AS parked,
         count(*) FILTER (WHERE lifecycle = 'CreditGrant') AS granted,
         count(*) FILTER (WHERE lifecycle = 'CreditPark')
           - count(*) FILTER (WHERE lifecycle = 'CreditGrant') AS outstanding
    FROM c
   GROUP BY device_index, kind, cost
  HAVING parked > granted
   ORDER BY outstanding DESC;
