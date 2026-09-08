#!/usr/bin/env python3
"""Exercise the real namespace controller and ServiceAccount admission.

Uses an existing non-system project namespace. Removes unique Pod/account
fixtures, restores the public CA after the fault probe, and requires an untouched
controller-default account before testing its delete/recreate behavior.
No tokens are requested and no containers are run.
"""
import argparse
import copy
import json
import subprocess
import time
import uuid
from pathlib import Path
from urllib.parse import quote


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubectl", type=Path, required=True)
    parser.add_argument("--kubeconfig", type=Path, required=True)
    parser.add_argument("--namespace", required=True)
    args = parser.parse_args()
    for path in [args.kubectl, args.kubeconfig]:
        if not path.is_absolute() or not path.is_file():
            parser.error("kubectl and kubeconfig must be existing absolute paths")
    if args.namespace in {"default", "kube-system", "kube-public", "kube-node-lease"}:
        parser.error("use an explicit non-system project namespace")
    base = f"/api/v1/namespaces/{quote(args.namespace, safe='')}"
    prefix = "api-sa-" + uuid.uuid4().hex[:10]
    created = []
    ca_original = None

    def call(*argv, value=None):
        return subprocess.run([str(args.kubectl), "--kubeconfig", str(args.kubeconfig),
                               "--request-timeout=15s", *argv], capture_output=True, text=True,
                              input=None if value is None else json.dumps(value), timeout=20)

    def run(*argv, value=None):
        result = call(*argv, value=value)
        if result.returncode:
            raise RuntimeError(f"kubectl {argv[0]} failed: {result.stderr.strip()}")
        return json.loads(result.stdout)

    def require(condition, message):
        if not condition:
            raise RuntimeError(message)

    def wait(path, predicate):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            result = call("get", "--raw", path)
            if result.returncode == 0:
                obj = json.loads(result.stdout)
                if predicate(obj):
                    return obj
            elif "NotFound" not in result.stderr:
                raise RuntimeError(result.stderr.strip())
            time.sleep(0.1)
        raise RuntimeError("controller did not converge within 15 seconds")

    def create(kind, plural, suffix, fields):
        name = prefix + "-" + suffix
        obj = run("create", "--raw", f"{base}/{plural}", "-f", "-",
                  value={"apiVersion": "v1", "kind": kind, "metadata": {"name": name}, **fields})
        path = f"{base}/{plural}/{name}"
        created.append((path, obj["metadata"]["uid"]))
        return obj

    def remove(path, uid):
        run("delete", "--raw", path, "-f", "-", value={"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid}})

    ca_path = f"{base}/configmaps/kube-root-ca.crt"
    try:
        account_path = f"{base}/serviceaccounts/default"
        default = wait(account_path, lambda _: True)
        for key in ["automountServiceAccountToken", "imagePullSecrets", "secrets"]:
            require(default.get(key) in [None, []], "default account has operator configuration; refusing repair probe")
        require(not default["metadata"].get("labels") and not default["metadata"].get("annotations"),
                "default account has operator metadata; refusing repair probe")
        ca_original = wait(ca_path, lambda v: "BEGIN CERTIFICATE" in v.get("data", {}).get("ca.crt", ""))
        remove(account_path, default["metadata"]["uid"])
        repaired = wait(account_path, lambda v: v["metadata"]["uid"] != default["metadata"]["uid"])
        damaged = copy.deepcopy(ca_original)
        damaged["data"]["ca.crt"] = "controlled-public-ca-fault"
        run("replace", "--raw", ca_path, "-f", "-", value=damaged)
        fixed = wait(ca_path, lambda v: v["data"]["ca.crt"] == ca_original["data"]["ca.crt"])
        require(fixed["metadata"]["uid"] == ca_original["metadata"]["uid"], "CA repair replaced object identity")
        custom = create("ServiceAccount", "serviceaccounts", "custom", {"automountServiceAccountToken": False,
                        "imagePullSecrets": [{"name": prefix + "-registry"}]})
        spec = {"securityContext": {"runAsNonRoot": True, "seccompProfile": {"type": "RuntimeDefault"}},
                "containers": [{"name": "pause", "image": "registry.k8s.io/pause:3.10",
                                "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}}}]}
        selected = copy.deepcopy(spec)
        selected["serviceAccountName"] = custom["metadata"]["name"]
        pod = create("Pod", "pods", "custom", {"spec": selected})
        require(not pod["spec"].get("volumes"), "account automount=false ignored")
        require(pod["spec"]["imagePullSecrets"] == custom["imagePullSecrets"], "account pull-secret defaults missing")
        projected = create("Pod", "pods", "default", {"spec": spec})
        require(projected["spec"]["serviceAccountName"] == "default", "default identity missing")
        require(any("serviceAccountToken" in source for volume in projected["spec"].get("volumes", [])
                    for source in volume.get("projected", {}).get("sources", [])), "token projection missing")
        denied_name = prefix + "-denied"
        missing = copy.deepcopy(spec)
        missing["serviceAccountName"] = prefix + "-missing"
        result = call("create", "--raw", f"{base}/pods", "-f", "-", value={"apiVersion": "v1", "kind": "Pod", "metadata": {"name": denied_name}, "spec": missing})
        if result.returncode == 0:
            obj = json.loads(result.stdout)
            created.append((f"{base}/pods/{denied_name}", obj["metadata"]["uid"]))
            raise RuntimeError("missing service account unexpectedly admitted")
        require("Forbidden" in result.stderr and "service account does not exist" in result.stderr, "unexpected denial reason")
        require(run("get", "--raw", f"{base}/pods?fieldSelector=metadata.name%3D{denied_name}")["items"] == [], "denied Pod persisted")
        print(json.dumps({"run_id": prefix, "result": "passed", "default_account_recreated": True,
                          "old_account_uid": default["metadata"]["uid"], "new_account_uid": repaired["metadata"]["uid"],
                          "public_ca_repaired": True, "serviceaccount_admission": "passed",
                          "token_issuance_verified": False, "containers_executed": False}))
    finally:
        cleanup_errors = []
        for path, uid in reversed(created):
            try:
                remove(path, uid)
            except Exception as error:
                cleanup_errors.append(str(error))
        # Always attempt CA restoration, even if removing a separate fixture
        # failed. Never report successful cleanup when a required action failed.
        if ca_original is not None:
            try:
                current = run("get", "--raw", ca_path)
                if current["data"]["ca.crt"] != ca_original["data"]["ca.crt"]:
                    require(current["metadata"]["uid"] == ca_original["metadata"]["uid"], "CA object changed identity; do not overwrite")
                    current["data"]["ca.crt"] = ca_original["data"]["ca.crt"]
                    run("replace", "--raw", ca_path, "-f", "-", value=current)
            except Exception as error:
                cleanup_errors.append(str(error))
        if cleanup_errors:
            raise RuntimeError("cleanup failed: " + "; ".join(cleanup_errors))
        print(json.dumps({"run_id": prefix, "cleanup": "passed", "removed": len(created),
                          "controller_default_account_and_ca_retained": True}))



if __name__ == "__main__":
    main()
