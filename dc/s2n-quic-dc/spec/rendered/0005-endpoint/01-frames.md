## Frames and their outcomes {#frames}

A frame is the unit of work a stream submits to the endpoint: a header that says what the frame is and which stream it is for, and an optional payload. The endpoint decides how frames are packed into packets. Every frame ends in one outcome that its writer can observe: acknowledged by the peer, or failed with a reason. The reasons are that the peer was declared dead, that the peer refused the packet, that the peer doesn't know the path secret, and that the writer cancelled the frame. Frames fall into priority classes, from acknowledgements through control frames to stream data.

### DC-0005-001 {#DC-0005-001}

A sender MAY carry several frames, of one or more streams to the same peer, in one packet.

### DC-0005-002 {#DC-0005-002}

The endpoint MUST report exactly one outcome for each frame to its writer: acknowledged, or failed with its reason.

### DC-0005-003 {#DC-0005-003}

The endpoint MUST NOT transmit a frame whose writer has cancelled it or gone away.

### DC-0005-004 {#DC-0005-004}

The endpoint MUST bound the number of transmissions of each frame, and report the frame as failed when it reaches that bound without an acknowledgement.

### DC-0005-005 {#DC-0005-005}

A sender MUST send a peer's pending frames of a higher priority class before its pending frames of a lower priority class.

### DC-0005-006 {#DC-0005-006}

A sender MUST NOT hold back an acknowledgement because the congestion window is full.
