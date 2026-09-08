# Stock-client wire fixture

`kubectl-deployment.pb` is the 317-byte Kubernetes Protobuf POST body produced by kubectl v1.34.11 (Linux ARM64), using `kubectl create deployment <unique-fixture-name> -n api-smoke --image=registry.k8s.io/pause:3.10 --replicas=2 -o json`. It was captured from verbose client output on 2026-09-08 while testing the project API.

SHA-256: `b6446b444352d08cfb18b9c24f77e729011fe78d19d94df3da4661f1fbedc7a9`. The body contains only synthetic object metadata and a public image reference; it contains no headers, certificates, tokens or credentials. The failed response was `422 invalid restartPolicy` before empty-string defaulting was corrected. The TLS/API regression test replays these unchanged bytes. No container was run by this capture.

The kubectl artifact was verified against SHA-256 `5b045a4712674c88a56fd98eef4285689738b7fbe8735e1b9ee3509521af5cb4` from the pinned tool installation.
