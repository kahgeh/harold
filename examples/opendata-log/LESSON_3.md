# Lesson 3: one saved position per agent

Run from `examples/opendata-log`:

```sh
cargo run --locked --offline --example lesson_3
```

Agent A has reports at positions 0 and 2; B has reports at 1 and 3.
The monitor reads A first and remembers **3** as A's next position. B's
position remains **0**, so reading B still finds both of its reports.

```text
Read agent-a from 0
  [0] Started
  [2] Finished
Read agent-b from 0
  [1] Started
  [3] Finished
Read agent-a from 3
Read agent-b from 4
Separate next positions: {"agent-a": 3, "agent-b": 4}
```

A single shared cursor would be a bug here: after reading A, scanning B
from 3 would skip B's `Started` report at 1. The key selects the stream;
the cursor is a lower bound in the shared log's sequence space.

Open [the example](examples/lesson_3.rs). `Checkpoints` is just a map from
stream key to next position. After successfully handling a record, the
monitor stores `record.sequence + 1`. Reading an empty stream leaves its
position unchanged. The two final scans demonstrate that neither stream
repeats already handled reports.

These positions are only in memory. Restarting this program forgets both
the database and the positions. [Lesson 4](LESSON_4.md) persists them
separately.

Try changing the final read order to B then A: each stream's results should
stay the same. The assertions check the final positions.
