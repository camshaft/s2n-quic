## Control packets {#control}

A control packet carries control frames from a stream's receiver to its sender: its tag's flags, its credentials and wire version, the stream identifier where it answers one stream, an optional source queue identifier, the packet number, the control data length, optional routing information, the control data and the authentication tag. The control data isn't encrypted; the authentication tag covers the whole packet.

### DC-0004-036 {#DC-0004-036}

In a control packet's tag, bit 0x08 MUST be set exactly when the packet carries a source queue identifier, bit 0x04 when it carries the identifier of the stream it answers, and bit 0x02 when it carries routing information.

### DC-0004-037 {#DC-0004-037}

A sender MUST set bit 0x01 of a control packet's tag to zero.

### DC-0004-038 {#DC-0004-038}

A control packet's fields MUST appear in this order: the tag, the credentials, the wire version, the stream identifier, the source queue identifier, the packet number, the control data length, the routing information, the control data and the authentication tag, each optional field only where its condition holds.

### DC-0004-039 {#DC-0004-039}

The stream identifier, source queue identifier, packet number and control data length of a control packet MUST be variable-length integers, the stream identifier encoded as a stream packet's is.

### DC-0004-040 {#DC-0004-040}

The control data of a control packet MUST be a sequence of QUIC frames.

### DC-0004-041 {#DC-0004-041}

A control packet MUST be authenticated and not encrypted: its authentication tag covers every byte from the packet's tag to the end of its control data, with the packet number as nonce and key phase zero.

### DC-0004-042 {#DC-0004-042}

The routing information of a control packet MUST be a routing type of 0 followed by the source sender identifier and the destination sender identifier, each a variable-length integer.

### DC-0004-043 {#DC-0004-043}

A receiver MUST NOT accept a control packet whose routing type is other than 0.
