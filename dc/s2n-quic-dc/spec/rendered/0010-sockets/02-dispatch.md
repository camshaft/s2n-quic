## Dispatching received packets {#dispatch}

The receiver decodes each datagram as one packet and passes it to the handler of its kind. A packet's handler gets the sender address and the congestion marking of its datagram. Datagrams that don't decode, and packets that no handler accepts, are dropped and logged. Where the receiver holds the path secret map, the secret control packets that report a stale key, a detected replay or an unknown path secret also reach that map.

### DC-0010-009 {#DC-0010-009}

The receiver MUST decode each datagram as one packet and pass it to the handler of its kind: control, stream, application datagram, queue reset, stale key, replay detected or unknown path secret.

### DC-0010-010 {#DC-0010-010}

Each packet passed to a handler MUST carry the sender address and the congestion marking of the datagram that held it.

### DC-0010-011 {#DC-0010-011}

A datagram that doesn't decode as a packet MUST be dropped without stopping the receipt of later datagrams.

### DC-0010-012 {#DC-0010-012}

A packet of a kind that no handler accepts MUST be dropped without stopping the receipt of later datagrams.

### DC-0010-013 {#DC-0010-013}

The receiver MUST ignore bytes of value zero that follow a packet up to the end of its datagram.

### DC-0010-014 {#DC-0010-014}

A receiver that holds a path secret map MUST give each stale key, replay detected and unknown path secret packet to that map before its handler.

### DC-0010-015 {#DC-0010-015}

A receiver SHOULD log each packet it drops, with the reason it dropped it.
