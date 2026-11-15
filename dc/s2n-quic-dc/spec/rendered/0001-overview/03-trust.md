## Trust model {#trust}

Each peer trusts the other as far as the handshake between them authenticated it, with the credentials that the deployment's handshake configuration asks for: a server that doesn't ask for the client's credentials derives path secrets with any client that reaches it. That handshake's session yields the path secret, from which both peers derive the keys of each later stream and datagram, and the stateless reset tokens each sends the other. A packet that doesn't authenticate under a key derived from a path secret comes from a host the system doesn't trust.

### DC-0001-014 {#DC-0001-014}

The system MUST derive a path secret only from a completed handshake with the peer that shares it.

### DC-0001-015 {#DC-0001-015}

A deployment SHOULD configure the handshake to authenticate both peers.

### DC-0001-016 {#DC-0001-016}

The system MUST NOT accept the data of a packet that doesn't authenticate under a key derived from a path secret it holds.

### DC-0001-017 {#DC-0001-017}

Once the handshake's path secret is derived, a client MUST send the server the stateless reset tokens it derives from that secret.

### DC-0001-018 {#DC-0001-018}

A server MUST send the client the stateless reset tokens it derives from the path secret once it has received the client's tokens.

### DC-0001-019 {#DC-0001-019}

Each peer MUST treat the handshake as complete for the dc protocol only once the other peer has confirmed that it received its stateless reset tokens.
