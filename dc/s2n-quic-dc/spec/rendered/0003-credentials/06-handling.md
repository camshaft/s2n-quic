## Handling keys and opened data {#handling}

Some keys protect one packet alone, such as the key of a single datagram. An opened payload is released only after the replay check that RFC 0002 states accepts it.

### DC-0003-043 {#DC-0003-043}

An endpoint MUST overwrite each secret, key-update secret, key and IV with zeros when it stops using it.

### DC-0003-044 {#DC-0003-044}

A single-use key MUST NOT seal more than one packet.

### DC-0003-045 {#DC-0003-045}

A single-use key MUST NOT open more than one packet.

### DC-0003-046 {#DC-0003-046}

A receiver MUST check an opened packet against replay protection after the AEAD verifies its tag and before it releases the payload.

### DC-0003-047 {#DC-0003-047}

A receiver whose replay check refuses an opened packet MUST overwrite the packet's decrypted payload with zeros.

### DC-0003-048 {#DC-0003-048}

An endpoint MUST NOT skip the replay check unless its operator turns the check off through a setting that is named as unsafe.
