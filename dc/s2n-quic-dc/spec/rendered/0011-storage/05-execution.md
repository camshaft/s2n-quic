## Execution and completion {#execution}

A device's dispatcher assigns each admitted operation to one of the device's lanes. The number of lanes is configured for each device, is at least one, and is independent of the device's queue depth. A lane runs operations through the execution backend and completes each operation where it finishes.

### DC-0011-026 {#DC-0011-026}

The dispatcher MUST choose the lane for each operation by comparing the queued work, in credits, of two candidate lanes.

### DC-0011-027 {#DC-0011-027}

The lane that finishes an operation MUST return its credit and notify its submitter without passing the operation to a central completion task.

### DC-0011-028 {#DC-0011-028}

When a lane has closed, the device MUST fail each operation routed to that lane with a broken-pipe error and return its credit.

### DC-0011-029 {#DC-0011-029}

When the device's submission channel has closed, the device MUST refuse a new operation with a broken-pipe error and return the credit it acquired for that operation.

### DC-0011-030 {#DC-0011-030}

A submitter MAY submit an operation without asking for its completion.
