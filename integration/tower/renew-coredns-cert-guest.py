#!/usr/bin/env python3
"""Replace the three-day CoreDNS client cert with a 365-day leaf. Guest-local only."""
import base64, datetime, json, os, pathlib, subprocess, tempfile, uuid

ROOT = pathlib.Path(os.environ.get("H3S_ROOT", "/var/lib/hedronetes"))
NS, NAME = "kube-system", "h3s-coredns-m1"
KUBE = [str(ROOT / "bin/kubectl"), "--kubeconfig", str(ROOT / "runtime/server/admin.kubeconfig")]
os.umask(0o077)
report = {
    "recorded_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "checks": {},
    "credential_lifetime_days": 365,
}


def run(args, data=None):
    r = subprocess.run(args, input=data, capture_output=True, timeout=30)
    if r.returncode:
        raise RuntimeError(args[0] + " failed: " + r.stderr.decode(errors="replace")[:700])
    return r.stdout


def kube_json(args):
    return json.loads(run(KUBE + args + ["-o", "json"]))


def check(name, value):
    report["checks"][name] = bool(value)
    if not value:
        raise AssertionError(name)


secret = kube_json(["-n", NS, "get", "secret", NAME])
raw = base64.b64decode(secret["data"]["kubeconfig"])
config = json.loads(raw)
user = config["users"][0]
cert_b64 = user["user"]["client-certificate-data"]
old_pem = base64.b64decode(cert_b64).decode()
identity = config["users"][0]["name"]
report["credential_identity"] = identity
old_end = run(["openssl", "x509", "-enddate", "-noout", "-in", "/dev/stdin"], old_pem.encode()).decode().strip()
report["previous_enddate"] = old_end
bundle = json.loads((ROOT / "runtime/server/tls/cluster-pki.json").read_text())
ca = bundle["ca"]["certificate_pem"]
with tempfile.TemporaryDirectory(prefix="coredns-renew-", dir="/tmp") as tmp:
    t = pathlib.Path(tmp)
    (t / "ca.pem").write_text(ca)
    (t / "ca.key").write_text(bundle["ca"]["private_key_pem"])
    del bundle
    run(["openssl", "genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", str(t / "client.key")])
    run(["openssl", "req", "-new", "-key", str(t / "client.key"), "-subj", "/CN=" + identity, "-out", str(t / "client.csr")])
    (t / "ext").write_text(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\n"
    )
    run(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(t / "client.csr"),
            "-CA",
            str(t / "ca.pem"),
            "-CAkey",
            str(t / "ca.key"),
            "-set_serial",
            "0x" + uuid.uuid4().hex,
            "-days",
            "365",
            "-extfile",
            str(t / "ext"),
            "-out",
            str(t / "client.pem"),
        ]
    )
    new_pem = (t / "client.pem").read_text()
    new_key = (t / "client.key").read_text()
    new_end = run(["openssl", "x509", "-enddate", "-noout", "-in", str(t / "client.pem")]).decode().strip()
    report["new_enddate"] = new_end
    user["user"]["client-certificate-data"] = base64.b64encode(new_pem.encode()).decode()
    user["user"]["client-key-data"] = base64.b64encode(new_key.encode()).decode()
    kubeconfig = json.dumps(config)
    patch = {"data": {"kubeconfig": base64.b64encode(kubeconfig.encode()).decode()}}
    run(KUBE + ["-n", NS, "patch", "secret", NAME, "--type=merge", "-p", json.dumps(patch)])
check("secret_patched", True)
# Remount credentials: delete Pod, controller/kubelet recreates? It's a naked Pod — recreate from live spec.
pod = kube_json(["-n", NS, "get", "pod", NAME])
spec = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": NAME, "namespace": NS, "labels": pod["metadata"].get("labels", {})}, "spec": pod["spec"]}
# Drop runtime-only fields that block create
spec["spec"].pop("nodeName", None)
run(KUBE + ["-n", NS, "delete", "pod", NAME, "--wait=true", "--timeout=60s"])
run(KUBE + ["create", "--validate=false", "-f", "-"], json.dumps(spec).encode())
import time

deadline = time.monotonic() + 120
ready = False
while time.monotonic() < deadline:
    p = kube_json(["-n", NS, "get", "pod", NAME])
    if any(c.get("type") == "Ready" and c.get("status") == "True" for c in p.get("status", {}).get("conditions", [])):
        ready = True
        break
    time.sleep(2)
check("coredns_ready_after_renewal", ready)
report["outcome"] = "passed"
print(json.dumps({k: report[k] for k in report if k != "credential_identity"}))
