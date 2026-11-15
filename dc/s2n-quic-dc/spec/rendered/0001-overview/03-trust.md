## Trust model {#trust}

Each peer trusts the other only after a handshake that authenticates it with the credentials the deployment configures. That handshake's session yields the path secret, from which both peers derive the keys of each later stream and datagram, and the stateless reset tokens each sends the other. A packet that doesn't authenticate under a key derived from a path secret comes from a host the system doesn't trust.

### DC-0001-012 {#DC-0001-012}

The system MUST derive a path secret only from a handshake that authenticated the peer with the credentials its deployment configures.

### DC-0001-013 {#DC-0001-013}

The system MUST NOT accept the data of a packet that doesn't authenticate under a key derived from a path secret it holds.

### DC-0001-014 {#DC-0001-014}

Once a handshake's path secret is derived, each peer MUST send the other peer the stateless reset tokens it derives from that secret.

### DC-0001-015 {#DC-0001-015}

Each peer MUST treat the handshake as complete for the dc protocol only once the other peer has confirmed that it received its stateless reset tokens.
