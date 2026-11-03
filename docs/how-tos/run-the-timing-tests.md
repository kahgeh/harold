# Run the timing tests

A timing test asserts a real time. For example, it checks that a task ends within a set number of milliseconds. A busy machine can make it fail.

## Default behavior

The plain `cargo test` command does not run the timing tests. CI does not run them either. Both show them as `ignored`, with the reason `timing: run with --features timing-tests`.

## Run them

Run this command on a quiet machine:

```sh
cargo test --workspace --locked --features harold/timing-tests,control-tower/timing-tests
```

`make release` runs the timing tests as part of its test step. If one fails, check that the machine is idle and run the command again.

## Why a quiet machine

A timing test measures real time. Other programs that use the CPU can slow the test and make it fail. Stop heavy work before you run the tests.
