## Distributing credit by priority {#distribution}

One distribution grants every waiting request of a pool. It runs in passes, each woken by a release or by the liveness refill. A pass serves the levels from highest to lowest, and the waiters of a level in arrival order. At the start of a level it computes one share: the pool's free credit divided by the count of the level's waiters, but never less than the level's smallest grant. It grants each waiter the lesser of its request and that share. Where the free credit can't cover what the head waiter is due, the pass stops, and every waiter behind it waits for the next pass. A holder whose grant was smaller than its request asks again and waits at the back of its level.

### DC-0008-016 {#DC-0008-016}

Exactly one distribution MUST grant credit to the waiting requests of a pool.

### DC-0008-017 {#DC-0008-017}

A pass MUST serve a lower priority level only once it has served every waiter of each higher level.

### DC-0008-018 {#DC-0008-018}

A pass MUST serve the waiters of one level in the order they began to wait.

### DC-0008-019 {#DC-0008-019}

At the start of each level, a pass MUST compute one share for all the level's waiters: the pool's free credit divided by their count, and at least the level's smallest grant.

### DC-0008-020 {#DC-0008-020}

A pass MUST grant each waiter the lesser of its request and its level's share.

### DC-0008-021 {#DC-0008-021}

Where the pool's free credit can't cover what the head waiter is due, a pass MUST stop, leaving that waiter and every waiter after it waiting.

### DC-0008-022 {#DC-0008-022}

A pool MUST return to its free credit the part of a request that a smaller grant leaves out.

### DC-0008-023 {#DC-0008-023}

A pass MUST wake the holder of each request it grants.

### DC-0008-024 {#DC-0008-024}

The distribution MUST grant at most a set budget of requests in one turn.

### DC-0008-025 {#DC-0008-025}

Where a turn ends on its budget while requests it could grant still wait, the distribution MUST run again without waiting for a release.
