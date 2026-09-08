#!/usr/bin/env python3
"""Extract patch metadata for our resources from pinned Kubernetes OpenAPI.

Usage: python3 generate-strategic-schema.py /path/to/swagger.json
Download URL and verified input hash are retained alongside the generated data.
No build-time network access or Go control-plane dependency is needed.
"""
import hashlib
import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1]).read_bytes()
assert hashlib.sha256(source).hexdigest() == "d3b0cdc2fda15c753206d25ab459dc7c12df64e2fd652b6809687471ea751c37", "unexpected Kubernetes schema"
definitions = json.loads(source)["definitions"]
roots = {
    **{kind: "io.k8s.api.core.v1." + kind for kind in
       ["Namespace", "ConfigMap", "Secret", "Pod", "Node", "Service", "ServiceAccount"]},
    **{kind: "io.k8s.api.apps.v1." + kind for kind in ["Deployment", "ReplicaSet"]},
    "EndpointSlice": "io.k8s.api.discovery.v1.EndpointSlice",
    "Lease": "io.k8s.api.coordination.v1.Lease",
    **{kind: "io.k8s.api.rbac.v1." + kind for kind in
       ["Role", "RoleBinding", "ClusterRole", "ClusterRoleBinding"]},
}
kept = {}


def compact(schema):
    result = {}
    for key in ["type", "$ref", "x-kubernetes-patch-strategy", "x-kubernetes-patch-merge-key"]:
        if key in schema:
            result[key] = schema[key]
    for key in ["items", "additionalProperties"]:
        if isinstance(schema.get(key), dict):
            result[key] = compact(schema[key])
    if "properties" in schema:
        result["properties"] = {key: compact(value) for key, value in schema["properties"].items()}
    if "$ref" in schema:
        visit(schema["$ref"].removeprefix("#/definitions/"))
    return result


def visit(name):
    if name not in kept:
        kept[name] = None
        kept[name] = compact(definitions[name])


for name in roots.values():
    visit(name)
output = {
    "source": "https://raw.githubusercontent.com/kubernetes/kubernetes/v1.34.11/api/openapi-spec/swagger.json",
    "source_sha256": hashlib.sha256(source).hexdigest(),
    "license": "Apache-2.0; Kubernetes Authors",
    "roots": roots,
    "definitions": kept,
}
path = pathlib.Path(__file__).with_name("strategic-schema.json")
path.write_text(json.dumps(output, sort_keys=True, separators=(",", ":")) + "\n")
print(f"{len(kept)} definitions, {path.stat().st_size} bytes")
