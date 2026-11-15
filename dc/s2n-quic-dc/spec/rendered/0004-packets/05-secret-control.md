## Secret control packets {#secret-control}

Four secret control packets report a problem with a path: an unknown path secret packet, a stale key packet, a replay detected packet and a queue reset packet. Each starts with its tag, a credential identifier or the credentials, and the wire version, and three of them can carry the identifier of the queue that is to receive them. Each ends with a 16-byte tag, and no secret control packet is longer than 64 bytes.

### DC-0004-044 {#DC-0004-044}

A secret control packet's tag MUST be 0x60 for an unknown path secret packet, 0x61 for a stale key packet, 0x62 for a replay detected packet and 0x67 for a queue reset packet, with bit 0x04 set on the first three exactly when the packet carries a queue identifier.

### DC-0004-045 {#DC-0004-045}

A secret control packet MUST NOT be longer than 64 bytes.

### DC-0004-046 {#DC-0004-046}

The queue identifier, minimum key identifier, rejected key identifier, code and trigger of a secret control packet MUST be variable-length integers.

### DC-0004-047 {#DC-0004-047}

An unknown path secret packet MUST hold, in this order, its tag, the credential identifier, the wire version, the queue identifier where the tag's bit 0x04 is set, and the 16-byte stateless reset token of that credential identifier.

### DC-0004-048 {#DC-0004-048}

A receiver MUST accept an unknown path secret packet only where its last 16 bytes equal the stateless reset token of its credential identifier, compared in constant time.

### DC-0004-049 {#DC-0004-049}

A stale key packet MUST hold, in this order, its tag, the credential identifier, the wire version, the sender's queue identifier where the tag's bit 0x04 is set, the minimum key identifier its sender still accepts, and a 16-byte authentication tag.

### DC-0004-050 {#DC-0004-050}

A replay detected packet MUST hold, in this order, its tag, the credential identifier, the wire version, the sender's queue identifier where the tag's bit 0x04 is set, the key identifier its sender rejected as a replay, and a 16-byte authentication tag.

### DC-0004-051 {#DC-0004-051}

A queue reset packet MUST hold, in this order, its tag, the credentials, the wire version, the queue identifier, a code, the trigger and a 16-byte authentication tag.

### DC-0004-052 {#DC-0004-052}

The trigger of a queue reset packet MUST be 0 where an unroutable stream packet caused the reset and 1 where an unroutable control packet caused it.

### DC-0004-053 {#DC-0004-053}

A receiver MUST read a queue reset trigger other than 0 and 1 as 0.

### DC-0004-054 {#DC-0004-054}

The 16-byte authentication tag of a stale key, replay detected or queue reset packet MUST be a message authentication code, under the path's control secret, of every byte before it.
