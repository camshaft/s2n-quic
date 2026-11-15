## Versions {#versions}

A dc version is a whole number. Each implementation holds the versions it supports in order of preference. The client lists its versions in a transport parameter of the handshake, the server selects one and answers with it in the same parameter, and both peers then speak the version the server selected. Where the server selects none, the connection doesn't use the dc protocol. Version 0 is the only version this specification defines.

### DC-0001-016 {#DC-0001-016}

A dc version MUST be a whole number from 0 to 4294967295.

### DC-0001-017 {#DC-0001-017}

A client MUST list the dc versions it supports, most preferred first and at most four of them, in the transport parameter 0xdc0000, each encoded as a variable-length integer.

### DC-0001-018 {#DC-0001-018}

A server MUST select the first of its own supported dc versions, in its order of preference, that the client's list holds.

### DC-0001-019 {#DC-0001-019}

A server that selects a dc version MUST send that version alone in the transport parameter 0xdc0000.

### DC-0001-020 {#DC-0001-020}

A server MUST read the first four versions of a client's list and ignore the versions after them.

### DC-0001-021 {#DC-0001-021}

A peer MUST NOT use the dc protocol on a connection where the server didn't select a dc version.

### DC-0001-022 {#DC-0001-022}

A peer that receives a dc-specific frame on a connection without a negotiated dc version MUST close the connection with an error.

### DC-0001-023 {#DC-0001-023}

Every implementation MUST support dc version 0.
