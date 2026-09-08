#!/usr/bin/env python3
"""Test workload API objects with stock kubectl; this does not run containers.

Uses an existing project namespace and removes its uniquely named objects with
UID preconditions. The synthetic Node is only a binding API fixture.
"""
import argparse
import json
import subprocess
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
    prefix = "api-workload-" + uuid.uuid4().hex[:10]
    ns = quote(args.namespace, safe="")
    created = []

    def run(*arguments, value=None):
        result = subprocess.run([str(args.kubectl), "--kubeconfig", str(args.kubeconfig),
                                 "--request-timeout=15s", *arguments], text=True,
                                input=None if value is None else json.dumps(value), capture_output=True, timeout=20)
        if result.returncode:
            raise RuntimeError(f"kubectl {arguments[0]} failed: {result.stderr.strip()}")
        return result.stdout

    def require(test, message):
        if not test:
            raise RuntimeError(message)

    def create(version, kind, plural, fields, cluster=False, name=None):
        name = name or prefix + "-" + plural
        metadata = {"name": name}
        if not cluster:
            metadata["namespace"] = args.namespace
        value = {"apiVersion": version, "kind": kind, "metadata": metadata, **fields}
        # Normal kubectl creation exercises its negotiated Protobuf write path.
        obj = json.loads(run("create", "--validate=false", "-f", "-", "-o", "json", value=value))
        base = "/api/v1" if version == "v1" else f"/apis/{version}"
        path = base + ("" if cluster else f"/namespaces/{ns}") + f"/{plural}/{quote(name, safe='')}"
        created.append((path, obj["metadata"]["uid"]))
        require(json.loads(run("get", "--raw", path)) == obj, f"{kind} read differs")
        return path, obj

    try:
        create("rbac.authorization.k8s.io/v1", "ClusterRole", "clusterroles", {"rules": []}, cluster=True, name="system:" + prefix)
        pod_spec = {"containers": [{"name": "pause", "image": "registry.k8s.io/pause:3.10"}]}
        _, node = create("v1", "Node", "nodes", {}, cluster=True)
        pod_path, pod = create("v1", "Pod", "pods", {"spec": pod_spec})
        require(pod["status"]["phase"] == "Pending", "new Pod must be Pending")
        selector = {"matchLabels": {"app": prefix}}
        template = {"metadata": {"labels": {"app": prefix}}, "spec": pod_spec}
        deployment_path, deployment = create("apps/v1", "Deployment", "deployments", {"spec": {"replicas": 2, "selector": selector, "template": template}})
        create("apps/v1", "ReplicaSet", "replicasets", {"spec": {"replicas": 2, "selector": selector, "template": template}})
        _, service = create("v1", "Service", "services", {"spec": {"selector": {"app": prefix}, "ports": [{"port": 80, "targetPort": 8080}]}})
        require(service["spec"]["clusterIP"].startswith("10.43."), "missing default ClusterIP allocation")
        create("v1", "ServiceAccount", "serviceaccounts", {})
        create("coordination.k8s.io/v1", "Lease", "leases", {"spec": {"holderIdentity": prefix, "leaseDurationSeconds": 40}})
        create("discovery.k8s.io/v1", "EndpointSlice", "endpointslices", {"addressType": "IPv4", "endpoints": [{"addresses": ["10.42.1.5"]}], "ports": [{"port": 8080}]})
        bound = json.loads(run("create", "--raw", pod_path + "/binding", "-f", "-", value={
            "apiVersion": "v1", "kind": "Binding", "metadata": {"name": pod["metadata"]["name"], "uid": pod["metadata"]["uid"]},
            "target": {"kind": "Node", "name": node["metadata"]["name"]}}))
        require(bound["status"] == "Success", "Pod binding failed")
        pod = json.loads(run("get", "--raw", pod_path))
        require(pod["spec"]["nodeName"] == node["metadata"]["name"], "Pod binding was not persisted")
        # Report observedGeneration without claiming replicas are running.
        deployment["status"] = {"observedGeneration": deployment["metadata"]["generation"]}
        reported = json.loads(run("replace", "--raw", deployment_path + "/status", "-f", "-", value=deployment))
        require(reported["status"]["observedGeneration"] == deployment["metadata"]["generation"], "status update failed")
        require(reported["spec"] == deployment["spec"], "status changed desired state")
        print(json.dumps({"run_id": prefix, "result": "passed", "api_object_count": len(created),
                          "checks": ["stock typed/Protobuf create and read", "Pod defaults and binding", "Deployment status", "ClusterIP assignment", "encoded RBAC name"],
                          "containers_executed": False, "worker_join_verified": False}))
    finally:
        for path, uid in reversed(created):
            run("delete", "--raw", path, "-f", "-", value={"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": uid}})
        if created:
            print(json.dumps({"run_id": prefix, "cleanup": "passed", "removed": len(created)}))


if __name__ == "__main__":
    main()
