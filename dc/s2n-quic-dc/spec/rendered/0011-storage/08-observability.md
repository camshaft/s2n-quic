## Load and observability {#observability}

Each device reports its load as committed credit, and keeps counters of its operations, bytes, failures, rejections and cancellations.

### DC-0011-042 {#DC-0011-042}

The device MUST report its load as its pool capacity less its available credit, counting the credit that waiting submitters ask for, and never less than zero.

### DC-0011-043 {#DC-0011-043}

The device MUST count, for each kind of operation, the operations submitted, the bytes moved and the operations that failed.

### DC-0011-044 {#DC-0011-044}

The device MUST count each operation it rejects at admission and each reservation cancelled before submission.
