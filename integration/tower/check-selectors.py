#!/usr/bin/env python3
"""Exercise stock kubectl against an explicitly selected project API.

Requires an existing namespace. Creates unique ConfigMaps and removes only
those whose UIDs match this run. Never reads or prints kubeconfig contents.
"""
import argparse
import json
import subprocess
import urllib.parse
import uuid
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubectl", required=True, type=Path)
    parser.add_argument("--kubeconfig", required=True, type=Path)
    parser.add_argument("--namespace", required=True)
    args = parser.parse_args()
    for path in [args.kubectl, args.kubeconfig]:
        if not path.is_absolute() or not path.is_file():
            parser.error("kubectl and kubeconfig must be explicit existing absolute paths")
    ns = urllib.parse.quote(args.namespace, safe="")
    collection = f"/api/v1/namespaces/{ns}/configmaps"
    run_id = "selector-" + uuid.uuid4().hex[:12]
    owned = {}

    def run(*arguments, value=None):
        command = [str(args.kubectl), "--kubeconfig", str(args.kubeconfig), "--request-timeout=15s", *arguments]
        completed = subprocess.run(command, input=None if value is None else json.dumps(value),
                                   text=True, capture_output=True, timeout=20)
        if completed.returncode:
            raise RuntimeError(f"kubectl {arguments[0]} failed: {completed.stderr.strip()}")
        return completed.stdout

    def raw(query):
        return json.loads(run("get", "--raw", collection + "?" + urllib.parse.urlencode(query)))

    def replace(obj, role, data):
        obj = json.loads(json.dumps(obj))
        obj["metadata"]["labels"]["role"] = role
        obj["data"]["value"] = data
        return json.loads(run("replace", "--validate=false", "-f", "-", "-o", "json", value=obj))

    def require(condition, message):
        if not condition:
            raise RuntimeError(message)

    try:
        objects = {}
        for suffix, role in [("a", "outside"), ("b", "web"), ("c", "web")]:
            name = f"{run_id}-{suffix}"
            obj = json.loads(run("create", "--validate=false", "-f", "-", "-o", "json", value={
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"namespace": args.namespace, "name": name,
                             "labels": {"h3s.io/selector-fixture": run_id, "role": role}},
                "data": {"value": "original"}}))
            owned[name] = obj["metadata"]["uid"]
            objects[suffix] = obj
        selector = f"h3s.io/selector-fixture={run_id},role=web"
        query = {"labelSelector": selector, "limit": 1}
        first = raw(query)
        require(first["items"] == [objects["b"]], "first filtered page differs")
        rv = first["metadata"]["resourceVersion"]
        changed_c = replace(objects["c"], "outside", "left")
        second = raw({**query, "continue": first["metadata"]["continue"]})
        require(second["items"] == [objects["c"]], "continuation lost original label snapshot")
        require(second["metadata"]["resourceVersion"] == rv and not second["metadata"].get("continue"),
                "continuation revision or completion differs")
        entered = replace(objects["a"], "web", "entered")
        changed = replace(entered, "web", "updated")
        exited = replace(objects["b"], "outside", "left")

        def deleted(previous, update):
            value = json.loads(json.dumps(previous))
            value["metadata"]["resourceVersion"] = update["metadata"]["resourceVersion"]
            return {"type": "DELETED", "object": value}

        expected = [deleted(objects["c"], changed_c), {"type": "ADDED", "object": entered},
                    {"type": "MODIFIED", "object": changed}, deleted(objects["b"], exited)]
        watch_path = collection + "?" + urllib.parse.urlencode({
            "watch": "true", "labelSelector": selector, "resourceVersion": rv, "timeoutSeconds": 1})
        events = [json.loads(line) for line in run("get", "--raw", watch_path).splitlines()]
        require(events == expected, "filtered watch transition values or order differ")
        # Exercise kubectl's own list pager as well as raw wire requests.
        listed = json.loads(run("get", "configmaps", "-n", args.namespace, "-l", selector,
                                "--chunk-size=1", "-o", "json"))
        require(listed["items"] == [changed], "stock kubectl selected list differs")
        named = raw({"fieldSelector": f"metadata.name={entered['metadata']['name']}"})
        require(named["items"] == [changed], "exact-name field selector differs")
        name = changed["metadata"]["name"]
        merged = json.loads(run("patch", "configmap", name, "-n", args.namespace,
                                "--type=merge", "-p", json.dumps({"data": {"value": None, "patched": "merge"}}),
                                "-o", "json"))
        require(merged["data"] == {"patched": "merge"}, "stock merge patch differs")
        operations = [{"op": "test", "path": "/metadata/resourceVersion",
                       "value": merged["metadata"]["resourceVersion"]},
                      {"op": "replace", "path": "/data/patched", "value": "json"}]
        patched = json.loads(run("patch", "configmap", name, "-n", args.namespace,
                                 "--type=json", "-p", json.dumps(operations), "-o", "json"))
        require(patched["data"] == {"patched": "json"}, "stock JSON patch differs")
        require(patched["metadata"]["uid"] == owned[name], "patch changed object identity")
        print(json.dumps({"run_id": run_id, "result": "passed", "snapshot_resource_version": rv,
                          "watch_types": [event["type"] for event in events],
                          "checks": ["filtered snapshot pagination", "watch selector transitions",
                                     "stock kubectl list pager", "exact-name field selection",
                                     "stock merge patch", "stock JSON patch with precondition"]}))
    finally:
        for name, uid in owned.items():
            # Enforce UID preconditions at the API, not a racy get-then-delete.
            run("delete", "--raw", collection + "/" + urllib.parse.quote(name, safe=""),
                "-f", "-", value={"apiVersion": "v1", "kind": "DeleteOptions",
                                   "preconditions": {"uid": uid}})
        if owned:
            print(json.dumps({"run_id": run_id, "cleanup": "passed", "removed": len(owned)}))


if __name__ == "__main__":
    main()
