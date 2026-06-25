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
-- These build on the Tier-1 base `events` view and the promoted key columns (cred_id, binding_id,
-- sender_id, packet_number, dump_id, free_request_id, dest_sender_id, msg_id). `event` is the
-- per-row event-type name; `lifecycle` lives inside each frame event's struct column, so it is
-- pulled out with the per-kind struct accessor below.

-- All frame events for one flow, unioned to a uniform shape, time-ordered. Each per-kind event
-- contributes its own `lifecycle` (out of its struct column) plus the common keys. queue_ids are
-- carried for context but never joined on.
CREATE OR REPLACE MACRO flow_frames(cred, bind) AS TABLE
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_data::QueueDataFrame".lifecycle AS lifecycle
    FROM events
    WHERE event_id = (SELECT event_id FROM events WHERE event LIKE '%QueueDataFrame' LIMIT 1)
      AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_msg::QueueMsgFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueMsgFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_max_data::QueueMaxDataFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueMaxDataFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_data_blocked::QueueDataBlockedFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueDataBlockedFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_reset::QueueResetFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueResetFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_dbg::QueueDbgFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueDbgFrame' AND cred_id = cred AND binding_id = bind
  UNION ALL BY NAME
  SELECT ts_nanos, event, cred_id, binding_id, sender_id, packet_number,
         "s2n_quic_dc::frame::queue_control::QueueControlFrame".lifecycle AS lifecycle
    FROM events
    WHERE event LIKE '%QueueControlFrame' AND cred_id = cred AND binding_id = bind
  ORDER BY ts_nanos;

-- A flow's packets: the `PacketRecord` rows whose (cred_id, sender_id, packet_number) appears on one
-- of the flow's frames. A packet is attributed to the flow THROUGH its frames (a packet may bundle
-- several flows' frames), not by cred_id alone. (`event` distinguishes the PacketEvent lifecycle —
-- RxArrived/Sent/Acked/Lost/RxDropped — inside the struct column; select it with the struct
-- accessor if needed.)
CREATE OR REPLACE MACRO flow_packets(cred, bind) AS TABLE
  SELECT p.ts_nanos, p.event, p.cred_id, p.sender_id, p.packet_number
    FROM "s2n_quic_dc::packet::PacketRecord" p
    WHERE p.cred_id = cred
      AND EXISTS (
        SELECT 1 FROM flow_frames(cred, bind) f
        WHERE f.sender_id = p.sender_id AND f.packet_number = p.packet_number
      )
    ORDER BY p.ts_nanos;

-- Whole flow timeline: frames + the packets they rode in, both ends, time-ordered.
CREATE OR REPLACE MACRO stream_timeline(cred, bind) AS TABLE
  SELECT ts_nanos, 'frame' AS kind, event, lifecycle, sender_id, packet_number
    FROM flow_frames(cred, bind)
  UNION ALL BY NAME
  SELECT ts_nanos, 'packet' AS kind, event, NULL AS lifecycle, sender_id, packet_number
    FROM flow_packets(cred, bind)
  ORDER BY ts_nanos;

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
