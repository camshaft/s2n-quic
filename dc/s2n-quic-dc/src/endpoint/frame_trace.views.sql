-- dc stream-correlation views (Tier-2 domain overlay for the frame flight recorder).
--
-- Registered with `backbeat::register_views!` from `frame_trace.rs`, so every `.bb` dump carries
-- this DDL and `backbeat convert` appends it after the generated Tier-1 views. Run the convert
-- sidecar against the Parquet (`duckdb out.parquet -init out.views.sql`) and these macros are ready.
--
-- Keys (see the design doc):
--   * flow key   = (cred_id, binding_id)            — one stream/flow. queue_ids are NOT keys.
--   * packet key = (cred_id, sender_id, packet_number)
-- A packet number is only unique within (cred_id, sender_id); cred_id is per-path, shared across
-- many flows, so it is never sufficient alone. packet_number/sender_id are SQL NULL on the
-- app-layer (AppSend/AppRecv) rows via the `sentinel = u64::MAX` declaration.
--
-- WHICH END a row came from: every row carries `instance_id` — a distinct per-process id stamped by
-- backbeat, so in a merged client+server dump it separates the two ends (the wire keys cred_id /
-- sender_id / binding_id are SHARED across both ends by design, so they cannot tell them apart;
-- instance_id can). When converting per host, pass `backbeat convert --host client:0` (the
-- `xtask local --frame-trace` tooling sets `BACKBEAT_HOST` to the node role) so a human label rides
-- in the Parquet footer; `instance_id` is the per-row join key.
--
-- These build on the Tier-1 base `events` view and its dense columns: `seq` (global order within the
-- converted file), `ts_nanos`, `event` (event-type name), `event_id`, `instance_id`, plus every
-- promoted key column (cred_id, binding_id, sender_id, packet_number, dump_id, free_request_id,
-- dest_sender_id, msg_id). `lifecycle` lives inside each frame event's struct column, pulled out
-- with the per-kind struct accessor below. Rows are ordered by (ts_nanos, seq) so a deterministic
-- simulated clock with ties still yields a stable, total order.

-- All frame events for one flow, unioned to a uniform shape, time-ordered. Each per-kind event
-- contributes its own `lifecycle` (out of its struct column) plus the common columns. queue_ids are
-- carried inside the per-kind struct for context but are never join keys.
CREATE OR REPLACE MACRO flow_frames(cred, bind) AS TABLE
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_data::QueueDataFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueDataFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_msg::QueueMsgFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueMsgFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_max_data::QueueMaxDataFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueMaxDataFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_data_blocked::QueueDataBlockedFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueDataBlockedFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_reset::QueueResetFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueResetFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_dbg::QueueDbgFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueDbgFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_control::QueueControlFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueControlFrame' AND cred_id = cred AND binding_id = bind
  ORDER BY ts_nanos, seq;

-- A flow's packets: the `PacketRecord` rows whose (cred_id, sender_id, packet_number) appears on one
-- of the flow's frames. A packet is attributed to the flow THROUGH its frames (a packet may bundle
-- several flows' frames), not by cred_id alone. The `event` column is the event-type name; the
-- PacketEvent lifecycle (RxArrived/Sent/Acked/Lost/RxDropped) is inside the struct column.
CREATE OR REPLACE MACRO flow_packets(cred, bind) AS TABLE
  SELECT p.seq, p.ts_nanos, p.instance_id, p.event, p.cred_id, p.sender_id, p.packet_number,
         "s2n_quic_dc::packet::PacketRecord".event AS packet_event
    FROM "s2n_quic_dc::packet::PacketRecord" p
    WHERE p.cred_id = cred
      AND EXISTS (
        SELECT 1 FROM flow_frames(cred, bind) f
        WHERE f.sender_id = p.sender_id AND f.packet_number = p.packet_number
      )
    ORDER BY p.ts_nanos, p.seq;

-- Whole flow timeline: frames + the packets they rode in, both ends, time-ordered. `instance_id`
-- tells the two ends apart in a merged dump; (ts_nanos, seq) gives a stable total order.
CREATE OR REPLACE MACRO stream_timeline(cred, bind) AS TABLE
  SELECT seq, ts_nanos, instance_id, 'frame' AS kind, event, lifecycle AS detail,
         sender_id, packet_number
    FROM flow_frames(cred, bind)
  UNION ALL BY NAME
  SELECT seq, ts_nanos, instance_id, 'packet' AS kind, event, packet_event AS detail,
         sender_id, packet_number
    FROM flow_packets(cred, bind)
  ORDER BY ts_nanos, seq;

-- dump_id -> flow identity (the QueueDbg frame is the bridge row), then the whole timeline. The
-- QueueDbgFrame carries both the dump_id and the flow key (cred_id, binding_id) directly.
CREATE OR REPLACE MACRO stream_by_dump(d) AS TABLE
  WITH s AS (
    SELECT cred_id, binding_id
    FROM "s2n_quic_dc::frame::queue_dbg::QueueDbgFrame"
    WHERE dump_id = d
    LIMIT 1
  )
  SELECT t.* FROM s, LATERAL stream_timeline(s.cred_id, s.binding_id) t;
