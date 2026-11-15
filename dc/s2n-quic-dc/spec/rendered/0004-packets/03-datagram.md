## Datagram packets {#datagram}

A datagram packet carries one application message: its tag's flags, its credentials and wire version, the port at which its sender receives secret control packets, an optional packet number, optional routing information, the payload length, an application header with its length where the payload is encrypted, the payload, and the authentication tag. Every byte before the payload is authenticated, and the payload is encrypted where the tag says so.

### DC-0004-026 {#DC-0004-026}

In a datagram packet's tag, bit 0x08 MUST be set exactly when the packet carries routing information, bit 0x04 when it carries a packet number, and bit 0x02 when its payload is encrypted.

### DC-0004-027 {#DC-0004-027}

Bit 0x01 of a datagram packet's tag MUST hold the key phase of the key that sealed the packet.

### DC-0004-028 {#DC-0004-028}

A datagram packet's fields MUST appear in this order: the tag, the credentials, the wire version, a 2-byte source control port, the packet number, the routing information, the payload length, the application header length, the application header, the payload and the authentication tag, each optional field only where its condition holds.

### DC-0004-029 {#DC-0004-029}

The packet number, the payload length and the application header length of a datagram packet MUST be variable-length integers.

### DC-0004-030 {#DC-0004-030}

A datagram packet MUST carry an application header length exactly when bit 0x02 of its tag is set, and a packet without one has an empty application header.

### DC-0004-031 {#DC-0004-031}

The nonce of a datagram packet's seal MUST be its packet number, or zero where the packet carries none.

### DC-0004-032 {#DC-0004-032}

The associated data of a datagram packet's seal MUST be every byte from its tag to the end of its application header.

### DC-0004-033 {#DC-0004-033}

The routing information of a datagram packet MUST be a routing type of 11 followed by the source sender identifier, each a variable-length integer.

### DC-0004-034 {#DC-0004-034}

A receiver MUST NOT accept a datagram packet whose routing type is other than 11.

### DC-0004-035 {#DC-0004-035}

The source control port of a datagram packet MUST be the port at which its sender receives secret control packets.
