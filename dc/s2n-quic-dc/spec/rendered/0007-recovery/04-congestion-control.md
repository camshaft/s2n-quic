## Congestion control {#congestion-control}

Each sender has one congestion controller, created with the path's maximum datagram size. The controller is model-based: it estimates the path's bottleneck bandwidth and its minimum round-trip time from the acknowledgements it receives, and sets the congestion window, the pacing rate and the send quantum from those estimates, with loss and congestion marks as further signals. A sender that has no more application data queued doesn't lower its rate or its window for the room it leaves unused, so a quiet sender isn't read as a slow path. A run of consecutive lost packet numbers counts as one loss event, and each increase of the peer's count of packets marked congestion experienced counts as a sign of congestion. The sender takes an acknowledgement's congestion marks and losses into account before it raises its rate or its window for the data that acknowledgement acknowledges. The sender doesn't treat a long silence as persistent congestion: the probe timer and the model cover it. The sender sends new data frames only while the window has room, as a fast retransmission, or when the frames make up a probe, and each send carries at most the send quantum. The room for new data is the window less the total size of the outstanding packets, and once a probe carries a packet's frames, the original no longer takes room.

### DC-0007-023 {#DC-0007-023}

The system MUST give each sender one window and one sending rate, set from the acknowledgements, losses and congestion marks of all of that sender's packets.

### DC-0007-024 {#DC-0007-024}

Except for data it sends as a fast retransmission or as a probe, the data a sender has sent and not yet seen acknowledged MUST stay within a window that the sender sets from the path's bottleneck bandwidth and minimum round-trip time, as it measures them from the acknowledgements it receives.

### DC-0007-040 {#DC-0007-040}

The rate at which a sender sends data MUST follow the path's bottleneck bandwidth, as the sender measures it from the acknowledgements it receives.

### DC-0007-025 {#DC-0007-025}

A sender MUST NOT lower its sending rate or its window because it sent less than its window allows while it had no application data queued.

### DC-0007-026 {#DC-0007-026}

A sender MUST respond to a run of consecutive lost packet numbers as it responds to one lost packet.

### DC-0007-027 {#DC-0007-027}

A sender MUST lower its sending rate or its window in response to an increase of the peer's count of packets marked congestion experienced.

### DC-0007-028 {#DC-0007-028}

The sending rate and the window a sender uses after it receives an acknowledgement MUST reflect that acknowledgement's losses and congestion marks as well as the data it acknowledges.

### DC-0007-029 {#DC-0007-029}

A sender MUST NOT send new data frames while the data it has sent and not yet seen acknowledged fills its window, unless it sends them as a fast retransmission or as a probe.

### DC-0007-030 {#DC-0007-030}

Each send of a sender MUST put at most its send quantum on the wire, counted in datagrams of the path's maximum size and rounded up, where the send quantum is the burst size the sender sets from its estimate of the path's bottleneck bandwidth.

### DC-0007-031 {#DC-0007-031}

The room a sender has for new data MUST be its window less the total size of its outstanding packets.

### DC-0007-032 {#DC-0007-032}

A packet whose frames a probe carries again MUST NOT take room in the window once the probe is sent.
