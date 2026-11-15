## Signals of a pool {#signals}

A pool reports gauges and counters under a prefix its holder sets, such as one prefix for the pool of sending and another for the pool of receiving. Its free credit reads below zero by the backlog of waiting requests, which is load, not an error. Its held credit is the leak signal: a large value under load is healthy, but held credit that stays above zero while traffic is idle and no request waits is credit that left and didn't come back. A gauge is computed when it is read, so a sample can be off by changes made while it is read; the steady value across samples is the signal.

### DC-0008-046 {#DC-0008-046}

A pool MUST report its signals under a prefix its holder sets.

### DC-0008-047 {#DC-0008-047}

A pool MUST report its free credit, which reads below zero by the credit waiting requests are owed.

### DC-0008-048 {#DC-0008-048}

A pool MUST report the credit that waiting requests are owed and haven't been granted.

### DC-0008-049 {#DC-0008-049}

A pool MUST report the credit returned by releases and not yet distributed.

### DC-0008-050 {#DC-0008-050}

A pool MUST report the credit its holders hold, as its capacity less its free, owed and returned credit.

### DC-0008-051 {#DC-0008-051}

A pool MUST count, at each priority level, the requests it granted at once and the requests that waited.

### DC-0008-052 {#DC-0008-052}

A pool MUST count the bytes acquired, the bytes released and the releases made.

### DC-0008-053 {#DC-0008-053}

A pool MUST count the distribution's passes, its grants, the cancelled waits it reclaimed, the races of a cancel and a grant, and the turns that ended on their budget.

### DC-0008-054 {#DC-0008-054}

A pool MUST report the credit a pass held back for the next pass, and count each clamp of its free credit to its capacity.

### DC-0008-055 {#DC-0008-055}

A pool MUST count the bytes the refill added, its ticks that added credit and its ticks that didn't, and report how many ticks in a row added credit.
