# Dependency scope for these lessons

The standalone manifest and lockfile pin OpenData Log 1.0.0, common 0.1.17,
SlateDB 0.13.1 and object_store 0.12.5. Do not substitute upstream-main snippets
without checking their APIs and dependency generation.

The September 20, 2026 audit approved the cached dependency set for local
experiments and bounded experiments against official AWS HTTPS endpoints. The
added direct serde 1.0.229 and serde_json 1.0.151 dependencies were already in the
lockfile. Tokio sync/time features do not introduce a new package version.

The dependency set is **not vulnerability-free**. `quick-xml` 0.38.4 has two
XML denial-of-service advisories, RUSTSEC-2026-0194 and RUSTSEC-2026-0195. The
S3/STS/session response parsers reach XML code; object payloads themselves are
not parsed as XML. Genuine AWS generates the response syntax, which constrains
exposure but does not fix the dependency. The fixed release is >=0.41.0, outside
object_store 0.12.5's `^0.38.0` requirement, so a lock-only update cannot resolve it.
The audit also reported bincode/paste maintenance notices.

The configuration helper rejects endpoint/TLS/conditional-put overrides for
cloud examples. Arbitrary S3-compatible servers and production deployment are
outside this tutorial's audit. Reassess dependency upgrades before extending
that scope. Do not disable TLS or conditional writes to make an experiment pass.

The local audit reports are kept separately under
`/Users/kahgeh/Dev/p/crates-audit/`, including
`opendata-agent-progress-cloud-2026-09-20.md`. This file records the portable
conclusion; it is not a clean `cargo audit` claim.
