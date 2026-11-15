## Reading a stream {#reading}

The read half reassembles what arrives into the order the peer wrote it, and delivers only contiguous bytes. It reports the end of the stream once the peer's end-of-stream signal and every byte before it are consumed. It limits how much the peer may send, and raises that limit as the application reads.

### DC-0006-008 {#DC-0006-008}

The read half MUST deliver bytes to the application in the order the peer wrote them, and hold data that arrives early until every byte before it has arrived.

### DC-0006-009 {#DC-0006-009}

The read half MUST report the end of the stream only once it has received the peer's end-of-stream signal and the application has consumed every byte before it.

### DC-0006-010 {#DC-0006-010}

Where the peer resets a stream after the read half has buffered some of its bytes, the read half MUST deliver those buffered bytes before it reports the reset.

### DC-0006-011 {#DC-0006-011}

The read half MUST raise the limit on how much the peer may send as the application consumes the data it received.

### DC-0006-012 {#DC-0006-012}

While the peer's write half keeps reporting that it is blocked on credit, the read half SHOULD raise the limit ahead of the peer's stated demand by about the data one round trip carries.

### DC-0006-013 {#DC-0006-013}

Where the application drops the read half before the end of the stream, the read half MUST ask the peer's write half to stop sending.

### DC-0006-014 {#DC-0006-014}

The read half MUST let the application learn that the peer's write half has ended or failed without consuming the data the read half holds.

### DC-0006-015 {#DC-0006-015}

The read half of a server's stream MUST tell the peer when it has consumed the end of the stream.
