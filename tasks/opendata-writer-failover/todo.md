# Understand single-writer failover

Explain single-writer capacity and safe takeover using the multi-agent progress example. Verify millisecond failover claims rather than assuming them. Research only; no runtime or dependency changes.

- [x] Read official SlateDB architecture, writes, readers, and fencing documentation.
- [x] Verify startup and recovery against the tutorial's installed SlateDB 0.13.1 and OpenData Log 1.0.0 source.
- [x] Explain producer versus storage-writer roles, fencing, recovery, durability, coordination, and end-to-end timing.

## Evidence

Official sources: https://slatedb.io/docs/get-started/introduction/, https://slatedb.io/docs/design/writes/, https://slatedb.io/docs/design/readers/, https://slatedb.io/rfcs/0001-manifest/.

No documented end-to-end millisecond failover guarantee found in the consulted sources. Distinguish current documentation from the tutorial's pinned versions. No failover benchmark had been run at this research stage. Lesson 9 now implements and locally verifies the exercise; see `examples/opendata-log/VERIFICATION.md` for measured results and limitations.

Independent source exploration verified SlateDB 0.13.1 at source commit `c20a2cc87f53ccaaa62d09aa8706a5a6ba8e99dc`: `slatedb/src/db/builder.rs:490-727` awaits manifest loading, writer epoch increment, WAL fencing, and WAL replay before returning. `slatedb/src/db.rs:297-329` fences via an empty WAL object; old writers fail conditional creation at the barrier. Optional cache preload also delays open. The full historical SST dataset need not be copied, but outstanding WAL recovery is not lazy in this version.

OpenData Log 1.0.0 additionally reconstructs sequence/segment state in `src/log.rs:393-402`. Neither inspected reader API exposes promotion to writer, nor provides a deployment-level failure detector/election/router. Opening a second writer against the same database path fences the first; separate agent keys do not create independent writer ownership.

The single writer orders/batches many producers. Safety relies on fencing plus durable acknowledgments; availability additionally requires application-level detection, promotion policy, routing and retries. Unknown write outcomes require stable application event IDs and a deduplication policy if duplicate effects matter. Suggested future learning exercise: pause rather than cleanly close writer A, promote B, resume A and verify fencing, and measure time to the first durable write through B. This was proposed at the research stage and subsequently implemented in lesson 9; AWS failover remains unverified.
