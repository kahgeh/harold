# Lesson 5: persist the log in S3 Standard

This lesson runs the same `durable_round_trip` as lesson 4. Log objects move
to S3; the monitor checkpoint stays on your machine. It requires an existing
general purpose bucket configured to use S3 Standard, in your selected
region. The example does not create buckets or set lifecycle/storage-class
policies. Check those bucket settings before comparing storage classes.

From this crate's directory, using a bucket you control:

```sh
unset AWS_S3_EXPRESS
export OPENDATA_BACKEND=s3
export AWS_REGION=ap-southeast-2
export AWS_BUCKET=your-general-purpose-bucket
export OPENDATA_PREFIX=opendata-lab/your-unique-trial/lesson-5
export OPENDATA_CHECKPOINT=.checkpoints/your-unique-trial-lesson-5.json
cargo run --locked --offline --example lesson_5
cargo run --locked --offline --example lesson_5
```

Replace the bucket and trial identifiers. The prefix must be an isolated
prefix for this database, and must be explicit. Opening a writer on an
existing database takes ownership and can fence an existing writer;
never point the lesson at a production prefix. Choose a different prefix
and checkpoint for each independent experiment.

Provide credentials through the Rust object-store client's supported AWS
credential chain, for example environment credentials including
`AWS_SESSION_TOKEN` for temporary credentials, or an EC2 role. Do not put
credentials in source, shell history, or committed files. An AWS CLI login
or profile alone does not guarantee that this Rust client's credential
provider can read it. The role needs the object and bucket operations used
by SlateDB, scoped to the experimental bucket/prefix, including listing,
reading, writing, and deletion for database maintenance.

`--offline` only prevents Cargo dependency downloads: the running example
still sends real S3 requests, stores objects, and incurs AWS charges. It
prints no credentials. Endpoint/TLS/conditional-put overrides are rejected
so that this lesson uses the audited genuine AWS configuration. Inherited
`SLATEDB_*` variables and `SlateDb.*` settings files are rejected to avoid
silently changing WAL durability settings.

On success the program prints the acknowledged batch position, replays both
reports after reopening, saves the checkpoint, and proves another reopen
has no unread reports. Running it again adds a new pair and handles them.
Old data remains in the bucket. A successful run is a concrete acceptance
result for those credentials, prefix, and backend; merely compiling it is
not an S3 test. Missing configuration fails before cloud access.

The checkpoint binds to the bucket/region/prefix configuration. To move the
monitor to another host, copy its checkpoint separately or deliberately
start from zero and deduplicate replay. OpenData does not persist your
consumer position for you.

AWS request latency, `flush` latency, and reader visibility are different
measurements. Lesson 10 separates them. A run from a Mac includes WAN
latency; it cannot establish in-region EC2 performance.

Cleanup is manual: stop all experiment writers/readers, remove only the
chosen trial prefix's objects, and remove its local checkpoint. Account for
bucket versioning when cleaning up general purpose buckets. No cleanup
command is supplied that could accidentally wipe a shared bucket.

Next: [concurrent agents and an independent monitor](LESSON_6.md).
