## Accepting streams {#accepting}

A server registers acceptors on its endpoint, each under an identifier. Each incoming stream goes to the acceptor its identifier names, and waits in a receiver's queue until the application takes it. A stream that arrives before any receiver has polled for one or registered to take one is turned away as busy. The application clones a receiver to add workers, and drops every receiver to retire the acceptor. A queue slot is the room the server holds for one stream of one client: the client opens each stream on a slot it holds, each write of the stream names that slot until the server confirms the stream, and the stream keeps its slot until the application has finished with it. The server then returns the freed slot to the client in a frame that names the slot, and the client may open another stream on it. The server starts each client with one slot, doubles the client's grant while its receivers take that client's streams as fast as they arrive, and holds it while streams wait or halves it by keeping freed slots. The memory that waiting streams hold is under pressure while the allocator or pool that holds them refuses or delays room for a stream; while it is, no grant grows and the server keeps freed slots.

### DC-0006-037 {#DC-0006-037}

A server MUST refuse to start where its endpoint and its handshake provider don't share one map of path secrets.

### DC-0006-038 {#DC-0006-038}

A server MUST deliver each incoming stream to the acceptor registered under the stream's acceptor identifier.

### DC-0006-039 {#DC-0006-039}

A server MUST refuse to register an acceptor under an identifier that is still registered, and report that the identifier is in use.

### DC-0006-040 {#DC-0006-040}

A server MUST let acceptors registered under different identifiers receive streams at the same time.

### DC-0006-041 {#DC-0006-041}

A server MUST reject a stream whose acceptor identifier doesn't name a registered acceptor.

### DC-0006-057 {#DC-0006-057}

Each receiver of an acceptor MUST keep every stream that waits for it until the application takes the stream or drops the receiver.

### DC-0006-058 {#DC-0006-058}

The server MUST NOT evict a waiting stream from a receiver's queue to make room for a new stream.

### DC-0006-059 {#DC-0006-059}

The server MUST NOT give a client back the credit of a stream until the application has finished with the stream.

### DC-0006-060 {#DC-0006-060}

The server MUST set the number of queue slots each client may hold for streams to that server.

### DC-0006-061 {#DC-0006-061}

At the handshake, the server MUST advertise to each client a starting grant of one queue slot.

### DC-0006-062 {#DC-0006-062}

The server MUST derive each client's grant of queue slots from how its acceptors' receivers take that client's streams and from the memory its waiting streams hold, and not from a fixed number.

### DC-0006-063 {#DC-0006-063}

Where a receiver takes a stream of a client that holds every slot of its grant, no stream of that client waits in any receiver's queue after the take, and the memory waiting streams hold isn't under pressure, the server MUST double that client's grant.

### DC-0006-064 {#DC-0006-064}

The server MUST NOT grow a client's grant while streams of that client wait in a receiver's queue.

### DC-0006-065 {#DC-0006-065}

While streams of a client wait in a receiver's queue, the server MAY shrink that client's grant by keeping freed slots instead of returning them to the client, down to half the grant the client held when its streams began to wait.

### DC-0006-066 {#DC-0006-066}

The server MUST tell a client of each slot it adds to the client's grant by naming that slot in the frame that returns freed slots to the client.

### DC-0006-067 {#DC-0006-067}

The client MUST take a slot that the server names as free and that the client hasn't held as a new slot of its grant.

### DC-0006-068 {#DC-0006-068}

The client MUST NOT open a stream on a queue slot that the server hasn't granted it.

### DC-0006-069 {#DC-0006-069}

The server MUST refuse a stream whose first write to reach the server names a queue slot that the server doesn't grant that client.

### DC-0006-070 {#DC-0006-070}

The server MUST keep each client's grant of queue slots at one slot or more.

### DC-0006-071 {#DC-0006-071}

The server MUST refuse a stream whose first write to reach the server names a queue slot that holds a stream the server hasn't freed.

### DC-0006-072 {#DC-0006-072}

While the memory that waiting streams hold is under pressure, the server MUST NOT grow any client's grant.

### DC-0006-073 {#DC-0006-073}

While the memory that waiting streams hold is under pressure, the server MUST keep each freed slot instead of returning it to its client, except the last slot of that client's grant.

### DC-0006-045 {#DC-0006-045}

Where an acceptor has several receivers, the server SHOULD give each stream to the less loaded of two receivers it picks at random.

### DC-0006-046 {#DC-0006-046}

The server MUST NOT give streams to a receiver that hasn't yet polled for a stream or registered to take one.

### DC-0006-054 {#DC-0006-054}

Where an acceptor has receivers and none of them has polled for a stream or registered to take one, the server MUST reset an incoming stream to that acceptor as busy.

### DC-0006-047 {#DC-0006-047}

Once every receiver of an acceptor is dropped, the server MUST reject streams to the acceptor's identifier.

### DC-0006-056 {#DC-0006-056}

Once a stream to an acceptor finds every receiver of the acceptor dropped, the server MUST remove the acceptor's registration.
