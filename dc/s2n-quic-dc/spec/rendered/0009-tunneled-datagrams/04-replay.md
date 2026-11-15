## Replay protection {#replay}

A receiver keeps a sliding window of the packet numbers it has accepted. A number in the window that the receiver accepted before is a repeat, and a number older than the window's oldest is too old: the receiver rejects both. A number newer than the window's newest moves the window on. The receiver records a number only for a packet whose tag verified.

### DC-0009-021 {#DC-0009-021}

A receiver MUST reject a packet whose packet number it accepted before.

### DC-0009-022 {#DC-0009-022}

A receiver MUST reject a packet whose packet number is older than the oldest number its replay window holds.

### DC-0009-023 {#DC-0009-023}

A receiver MUST accept, in its replay check, a packet whose packet number is newer than every number it accepted before.

### DC-0009-024 {#DC-0009-024}

A receiver MUST record a packet number in its replay window only once the packet's authentication tag verifies.
