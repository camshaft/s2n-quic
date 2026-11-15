## Receiving {#receiving}

A receiver decodes a packet from the front of a datagram and leaves the bytes after it for the next packet. It rejects a packet whose first byte doesn't mark a datagram packet, or marks routing information or an application header. It opens the payload with the key the credentials identify, with the packet number as the nonce and the header as associated data, and hands the payload to the application only once the authentication tag verifies.

### DC-0009-015 {#DC-0009-015}

A receiver MUST reject a packet whose first byte doesn't hold 0100 in its high four bits.

### DC-0009-016 {#DC-0009-016}

A receiver MUST reject a packet whose routing information bit is set.

### DC-0009-017 {#DC-0009-017}

A receiver MUST reject a packet whose application header bit is set.

### DC-0009-018 {#DC-0009-018}

A receiver MUST read the last 16 bytes of each packet as its authentication tag.

### DC-0009-019 {#DC-0009-019}

A receiver MUST NOT hand a payload to the application unless the packet's authentication tag verifies over its header and its payload, under the key its credentials identify and with its packet number as the nonce.

### DC-0009-020 {#DC-0009-020}

A datagram MAY carry more than one tunneled datagram packet, each starting at the byte after the end of the one before it.
