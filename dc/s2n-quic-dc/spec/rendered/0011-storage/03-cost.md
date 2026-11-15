## Cost and pools {#cost}

Each device has a cost model and a weight for each kind of operation. Under the bytes model, an operation's base cost is its length in bytes. Under the operations model, it is its length divided by the configured operation unit, rounded up, and a control operation such as a flush costs one. The base cost is multiplied by the weight of the operation's kind. A device holds either one shared pool for all operations, or two pools: one for reads and one for every other kind of operation.

### DC-0011-013 {#DC-0011-013}

Under the bytes cost model, the device MUST compute an operation's base cost as its length in bytes, and as one for an operation of length zero.

### DC-0011-014 {#DC-0011-014}

Under the operations cost model, the device MUST compute an operation's base cost as its length divided by the configured operation unit, rounded up, and as one for a control operation or an operation of length zero.

### DC-0011-015 {#DC-0011-015}

The device MUST multiply an operation's base cost by the configured weight of its kind and round the result to a whole number of credits no smaller than one.

### DC-0011-016 {#DC-0011-016}

When an operator does not configure the weights, the device SHOULD weight a read at one and a write at two.

### DC-0011-017 {#DC-0011-017}

In split pool mode, the device MUST draw a read's credit from the read pool and the credit of every other kind of operation from the write pool.

### DC-0011-018 {#DC-0011-018}

In shared pool mode, the device MUST draw the credit of every operation from one pool.
