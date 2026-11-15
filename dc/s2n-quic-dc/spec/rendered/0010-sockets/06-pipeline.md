## Pipeline channels {#pipeline}

Received and sent items pass between stages through channels. Each poll cycle of a pipeline has a budget of items: a stage processes items until its budget runs out and then yields, and the pipeline wakes itself once where items remain. A priority merge serves its higher-priority inputs first, and a stage that spreads items over several outputs takes them in turn.

### DC-0010-037 {#DC-0010-037}

Each poll of a pipeline stage MUST process at most the items its budget allows and then yield.

### DC-0010-038 {#DC-0010-038}

A pipeline whose poll ends with items left MUST wake itself once to continue.

### DC-0010-039 {#DC-0010-039}

A priority merge MUST take an item from a higher-priority input whenever one is ready, before it takes one from a lower-priority input.

### DC-0010-040 {#DC-0010-040}

An item a priority merge reads ahead MUST be delivered by its next receive.

### DC-0010-041 {#DC-0010-041}

A socket sender stage that fails to send an item MUST return the item with the error, so that the caller releases it.

### DC-0010-042 {#DC-0010-042}

A stage that spreads items across several outputs SHOULD give each item to the next output in turn after the one it used last.
