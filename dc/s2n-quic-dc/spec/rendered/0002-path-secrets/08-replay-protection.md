## Replay protection {#replay-protection}

A receiver tracks, for each path secret, the highest key identifier it has seen and a window of the key identifiers below it, with which of them it has seen. Before a path secret has seen any key identifier, the lowest key identifier it would accept is 0.

### DC-0002-058 {#DC-0002-058}

An endpoint MUST record a received key identifier as seen only after the packet that carries it is authenticated.

### DC-0002-059 {#DC-0002-059}

An endpoint MUST track, for each path secret, which key identifiers it has seen among at least the 896 below the highest one it has seen.

### DC-0002-060 {#DC-0002-060}

An endpoint MUST reject a packet as a replay where it has already recorded the packet's key identifier as seen under that path secret.

### DC-0002-061 {#DC-0002-061}

An endpoint MUST reject a packet as a possible replay where the packet's key identifier is below the window it tracks.

### DC-0002-062 {#DC-0002-062}

An endpoint MUST answer a packet it rejects as a replay with a replay detected packet that carries the rejected key identifier.

### DC-0002-063 {#DC-0002-063}

An endpoint MUST answer a packet it rejects as a possible replay with a stale key packet that carries the lowest key identifier it would accept.

### DC-0002-064 {#DC-0002-064}

An endpoint MUST NOT deliver the payload of a packet it rejects as a replay or a possible replay.
