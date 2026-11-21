## Ordered reads across devices {#ordered-reads}

An ordered read takes a sequence of blocks. A block is either a range of a file on a device, with a number of bytes to trim from its head and from its tail, or bytes already in memory. The ordered read reads the device blocks, buffered or direct, and delivers each block's bytes in the order of the sequence. Its window holds every block it has taken from the sequence and not yet delivered: blocks waiting for credit, blocks being read, and blocks read and waiting for their turn. The window bound is the number of blocks the window can hold. It starts at one block, or at a starting value the read's configuration sets, and changes at run time from the devices' answers to the read: it rises when the read waits on a front block whose read hasn't completed while the window is full and every device block of the window has been submitted, to twice its value until the read first lowers it and by one block after that, and it halves, rounded up, at a would-block answer to one of the read's reads. The bound's last change is its latest raise or decrease, or the start of the read where the bound hasn't changed yet, and its last decrease is its latest decrease, or the start of the read where it hasn't been lowered yet. A read's configuration doesn't set an upper bound on the window bound: only the devices' answers stop it from rising. Blocks past the bound stay in the sequence until the window has room for them.

### DC-0011-036 {#DC-0011-036}

An ordered read MUST deliver the blocks' bytes in the order of the block sequence, whatever order their reads complete in.

### DC-0011-037 {#DC-0011-037}

An ordered read MUST NOT let a block waiting for credit on one device delay the submission of a block on another device.

### DC-0011-038 {#DC-0011-038}

An ordered read MUST deliver a block of in-memory bytes in its place in the order, without reading a device or acquiring credit.

### DC-0011-039 {#DC-0011-039}

An ordered read MUST remove a device block's head trim and tail trim from the bytes it delivers for that block.

### DC-0011-040 {#DC-0011-040}

An ordered read MUST deliver a block's validation error in that block's place in the order.

### DC-0011-041 {#DC-0011-041}

An ordered read MUST bound the number of blocks it has taken from the sequence and not yet delivered.

### DC-0011-070 {#DC-0011-070}

An ordered read MUST start its window bound at one block, or at the starting value its configuration sets where it sets one.

### DC-0011-071 {#DC-0011-071}

An ordered read MUST raise its window bound when it waits on a front block whose read hasn't completed while its window holds as many blocks as the bound, every device block of its window has been submitted to its device, and no device has answered a read of the ordered read with would-block since the bound's last change: to twice its value where the read hasn't lowered the bound yet, and by one block where it has.

### DC-0011-072 {#DC-0011-072}

On a would-block answer to a read of one of its blocks, an ordered read MUST lower its window bound to half its value, rounded up.

### DC-0011-073 {#DC-0011-073}

An ordered read MUST NOT lower its window bound for a would-block answer to a read that it submitted before the bound's last decrease.

### DC-0011-074 {#DC-0011-074}

An ordered read MUST NOT cancel or drop a block it has taken from the sequence when it lowers its window bound.

### DC-0011-075 {#DC-0011-075}

An ordered read MUST NOT take a block from the sequence while its window holds as many blocks as its window bound, or more.

### DC-0011-076 {#DC-0011-076}

An ordered read MUST take the next block from the sequence while its window holds fewer blocks than its window bound and the sequence holds a block it hasn't taken.

### DC-0011-077 {#DC-0011-077}

An ordered read MUST change its window bound only as DC-0011-071 and DC-0011-072 require.
