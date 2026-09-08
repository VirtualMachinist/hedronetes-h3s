# Development-only strategic patch oracle

This Go tool computes the expected JSON or error using upstream Kubernetes
v0.34.11. It is not part of the h3s build/runtime and adds no Go control-plane
dependency. `go.mod` and `go.sum` pin the test generator's dependencies.

From the h3s repository root:

```sh
go -C crates/h3s-apiserver/tests/fixtures/strategic-oracle run . \
  < crates/h3s-apiserver/tests/fixtures/strategic-v1.34.11.json \
  > /tmp/h3s-strategic-regenerated.json
cmp crates/h3s-apiserver/tests/fixtures/strategic-v1.34.11.json \
  /tmp/h3s-strategic-regenerated.json
cargo test -p h3s-apiserver --lib --test nodes --test bootstrap --locked
```

Always generate to a different output file: redirecting over the input destroys
the corpus before the process reads it. A comparison change requires review;
never regenerate expected results from the Rust implementation under test.

The 110 cases include fixed API examples, permutations, and 64 deterministic
condition-update/delete/order combinations generated with seed 20260908. They
are a bounded differential corpus, not exhaustive conformance coverage.
