## Events {#events}

The endpoint reports what it does as events. Each event names one thing that happened, such as a datagram sent, a key accepted or a connection that timed out. An event carries the time it happened and its subject: the endpoint as a whole, or one connection, named by an identifier that is unique within the process. The application supplies a subscriber when it creates the endpoint, and the subscriber receives every event. An application that supplies none gets a subscriber that discards them.

### DC-0013-001 {#DC-0013-001}

The endpoint MUST deliver each event it reports to the subscriber that the application supplied when it created the endpoint.

### DC-0013-002 {#DC-0013-002}

Each event MUST carry the time at which it happened and its subject, which is either the endpoint as a whole or one connection.

### DC-0013-003 {#DC-0013-003}

The system MUST assign each connection an identifier that no earlier connection of the same process held, counting up from 1.

### DC-0013-004 {#DC-0013-004}

The endpoint MUST NOT change its protocol behavior according to whether a subscriber is present or what the subscriber does with an event.

### DC-0013-005 {#DC-0013-005}

An endpoint created without a subscriber MUST use a subscriber that discards every event.

### DC-0013-006 {#DC-0013-006}

The endpoint SHOULD report an event without waiting for the subscriber to write or send anything.
