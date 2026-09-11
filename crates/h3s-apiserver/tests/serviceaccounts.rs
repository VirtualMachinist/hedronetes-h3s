mod common;
use common::Server;
use serde_json::{json, Value};
use std::time::Duration;

fn pod(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{"securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}})
}
async fn account(s: &Server, name: &str, fields: Value) -> Value {
    let mut value = json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name}});
    for (k, v) in fields.as_object().unwrap() {
        value[k] = v.clone();
    }
    let (code, value) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/serviceaccounts",
            value,
        )
        .await;
    assert_eq!(code, 201, "{value}");
    value
}
async fn create(s: &Server, value: Value) -> (u16, Value) {
    s.json(s.admin(), "POST", "/api/v1/namespaces/team-a/pods", value)
        .await
}
async fn wait_object(s: &Server, path: &str, condition: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (code, v) = s.json(s.admin(), "GET", path, json!({})).await;
            if code == 200 && condition(&v) {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("controller did not converge")
}
#[tokio::test]
async fn controller_creates_repairs_and_preserves_namespace_identity_resources() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_controllers(dir.path()).await;
    s.namespace("team-a").await;
    let sa = "/api/v1/namespaces/team-a/serviceaccounts/default";
    let cm = "/api/v1/namespaces/team-a/configmaps/kube-root-ca.crt";
    let (_, initial) = s.json(s.admin(), "GET", sa, json!({})).await;
    let ca = wait_object(&s, cm, |v| v["data"]["ca.crt"] == s.pki.ca_pem()).await;
    assert_eq!(s.patch(s.admin(),sa,"application/merge-patch+json",json!({"automountServiceAccountToken":false,"imagePullSecrets":[{"name":"registry"}],"metadata":{"labels":{"operator":"owned"}}})).await.0,200);
    assert_eq!(
        s.patch(
            s.admin(),
            cm,
            "application/merge-patch+json",
            json!({"data":{"ca.crt":"wrong","extra":"preserve"}})
        )
        .await
        .0,
        200
    );
    let fixed = wait_object(&s, cm, |v| v["data"]["ca.crt"] == s.pki.ca_pem()).await;
    assert_eq!(fixed["metadata"]["uid"], ca["metadata"]["uid"]);
    assert_eq!(fixed["data"]["extra"], "preserve");
    let (_, account) = s.json(s.admin(), "GET", sa, json!({})).await;
    assert_eq!(account["metadata"]["uid"], initial["metadata"]["uid"]);
    assert_eq!(account["automountServiceAccountToken"], false);
    assert_eq!(account["metadata"]["labels"]["operator"], "owned");
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            sa,
            json!({"preconditions":{"uid":initial["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    let replaced = wait_object(&s, sa, |v| {
        v["metadata"]["uid"] != initial["metadata"]["uid"]
    })
    .await;
    assert!(replaced["automountServiceAccountToken"].is_null());
    drop(s);
    let s = Server::start_with_controllers(dir.path()).await;
    assert_eq!(
        wait_object(&s, sa, |_| true).await["metadata"]["uid"],
        replaced["metadata"]["uid"]
    );
    assert_eq!(
        wait_object(&s, cm, |v| v["data"]["ca.crt"] == s.pki.ca_pem()).await["metadata"]["uid"],
        ca["metadata"]["uid"]
    );
    assert_eq!(
        s.patch(
            s.admin(),
            "/api/v1/namespaces/team-a/status",
            "application/merge-patch+json",
            json!({"status":{"phase":"Terminating"}})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            sa,
            json!({"preconditions":{"uid":replaced["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.json(s.admin(), "GET", sa, json!({})).await.0, 404);
}
#[tokio::test]
async fn controller_credentials_have_only_required_api_access() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_controllers(dir.path()).await;
    s.namespace("team-a").await;
    let cert = s
        .pki
        .issue_client(h3s_controllers::NAMESPACE_CONTROLLER_ID, None)
        .unwrap();
    let client = || s.pki.client_config(Some(&cert)).unwrap();
    for path in [
        "/api/v1/namespaces",
        "/api/v1/serviceaccounts?fieldSelector=metadata.name%3Ddefault",
        "/api/v1/configmaps?fieldSelector=metadata.name%3Dkube-root-ca.crt",
        "/api/v1/secrets",
        "/api/v1/pods",
        "/api/v1/configmaps",
        "/api/v1/serviceaccounts",
        "/api/v1/services",
    ] {
        assert_eq!(
            s.json(client(), "GET", path, json!({})).await.0,
            200,
            "{path}"
        );
    }
    for resource in [
        "secrets",
        "pods",
        "configmaps",
        "serviceaccounts",
        "services",
    ] {
        let path = format!("/api/v1/namespaces/team-a/{resource}/gc-probe");
        assert_eq!(
            s.json(client(), "DELETE", &path, json!({})).await.0,
            404,
            "{path}"
        );
    }
    for path in [
        "/apis/rbac.authorization.k8s.io/v1/clusterroles",
        "/api/v1/namespaces/team-a/secrets/gc-probe",
    ] {
        assert_eq!(
            s.json(client(), "GET", path, json!({})).await.0,
            403,
            "{path}"
        );
    }
    assert_eq!(
        s.json(
            client(),
            "POST",
            "/api/v1/namespaces/team-a/pods",
            pod("bad")
        )
        .await
        .0,
        403
    );
}
#[tokio::test]
async fn account_resolution_requires_same_namespace_and_consistent_names() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.namespace("team-b").await;
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-b/serviceaccounts",
            json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"other"}})
        )
        .await
        .0,
        201
    );
    for name in ["missing", "other"] {
        let mut value = pod(name);
        value["spec"]["serviceAccountName"] = json!(name);
        let (code, response) = create(&s, value).await;
        assert_eq!(code, 403, "{response}");
        assert_eq!(
            s.json(
                s.admin(),
                "GET",
                &format!("/api/v1/namespaces/team-a/pods/{name}"),
                json!({})
            )
            .await
            .0,
            404
        );
    }
    account(&s, "custom", json!({"automountServiceAccountToken":false})).await;
    let mut value = pod("alias");
    value["spec"]["serviceAccount"] = json!("custom");
    let (code, created) = create(&s, value).await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["spec"]["serviceAccountName"], "custom");
    assert!(created["spec"]["volumes"].is_null());
    let mut value = pod("conflict");
    value["spec"]["serviceAccount"] = json!("custom");
    value["spec"]["serviceAccountName"] = json!("default");
    assert_eq!(create(&s, value).await.0, 422);
    let mut value = pod("path");
    value["spec"]["serviceAccountName"] = json!("../team-b/other");
    assert_eq!(create(&s, value).await.0, 422);
    let mut value = pod("mirror");
    value["metadata"]["annotations"] = json!({"kubernetes.io/config.mirror":"fake"});
    assert_eq!(create(&s, value).await.0, 403);
}
#[tokio::test]
async fn accounts_never_project_a_token_and_pull_secrets_are_outside_the_profile() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    account(&s, "mounting", json!({"automountServiceAccountToken":true})).await;
    account(
        &s,
        "pulling",
        json!({"imagePullSecrets":[{"name":"registry"}]}),
    )
    .await;
    // The API default is false even when the account asks for a token: the
    // node would not mount it and the API could not authenticate it.
    for name in ["default", "mounting"] {
        let mut value = pod(&format!("via-{name}"));
        value["spec"]["serviceAccountName"] = json!(name);
        let (code, created) = create(&s, value).await;
        assert_eq!(code, 201, "{created}");
        assert_eq!(created["spec"]["serviceAccountName"], name);
        assert_eq!(created["spec"]["automountServiceAccountToken"], false);
        assert_eq!(created["spec"]["enableServiceLinks"], false);
        assert!(created["spec"].get("volumes").is_none(), "{created}");
        assert!(
            created["spec"]["containers"][0]
                .get("volumeMounts")
                .is_none(),
            "{created}"
        );
    }
    // An explicit token request, inherited pull secrets, or the Pod's own
    // pull secrets leave the runtime profile: refused, never persisted.
    let mut value = pod("explicit");
    value["spec"]["automountServiceAccountToken"] = json!(true);
    let (code, refused) = create(&s, value).await;
    assert_eq!(code, 422, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("token projection"),
        "{refused}"
    );
    let mut value = pod("pulls");
    value["spec"]["serviceAccountName"] = json!("pulling");
    let (code, refused) = create(&s, value).await;
    assert_eq!(code, 422, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("image authentication"),
        "{refused}"
    );
    let mut value = pod("own-pull");
    value["spec"]["imagePullSecrets"] = json!([{"name":"own"}]);
    assert_eq!(create(&s, value).await.0, 422);
    for name in ["explicit", "pulls", "own-pull"] {
        assert_eq!(
            s.json(
                s.admin(),
                "GET",
                &format!("/api/v1/namespaces/team-a/pods/{name}"),
                json!({})
            )
            .await
            .0,
            404
        );
    }
    // A Pod-declared volume at the token path is an ordinary volume.
    let mut value = pod("mount-override");
    value["spec"]["volumes"] = json!([{"name":"own","configMap":{"name":"own"}}]);
    value["spec"]["containers"][0]["volumeMounts"] =
        json!([{"name":"own","mountPath":"/var/run/secrets/kubernetes.io/serviceaccount"}]);
    let (code, created) = create(&s, value).await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["spec"]["volumes"].as_array().unwrap().len(), 1);
}
#[tokio::test]
async fn mountable_secret_policy_covers_volumes_environment_and_pull_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    account(&s,"limited",json!({"metadata":{"name":"limited","annotations":{"kubernetes.io/enforce-mountable-secrets":"true"}},"secrets":[{"name":"allowed"}]})).await;
    for (i, spec) in [
        json!({"volumes":[{"name":"s","secret":{"secretName":"denied"}}]}),
        json!({"volumes":[{"name":"s","projected":{"sources":[{"secret":{"name":"denied"}}]}}]}),
        json!({"imagePullSecrets":[{"name":"denied"}]}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = pod(&format!("denied-{i}"));
        value["spec"]["serviceAccountName"] = json!("limited");
        for (k, v) in spec.as_object().unwrap() {
            value["spec"][k] = v.clone();
        }
        let (code, response) = create(&s, value).await;
        assert_eq!(code, 403, "{response}");
    }
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for env in [
            json!({"env":[{"name":"S","valueFrom":{"secretKeyRef":{"name":"denied","key":"key"}}}]}),
            json!({"envFrom":[{"secretRef":{"name":"denied"}}]}),
        ] {
            let mut value = pod("env");
            value["spec"]["serviceAccountName"] = json!("limited");
            let mut c = value["spec"]["containers"][0].clone();
            c["name"] = json!("check");
            for (k, v) in env.as_object().unwrap() {
                c[k] = v.clone();
            }
            value["spec"][field] = json!([c]);
            assert_eq!(create(&s, value).await.0, 403);
        }
    }
    let mut value = pod("allowed");
    value["spec"]["serviceAccountName"] = json!("limited");
    value["spec"]["volumes"] = json!([{"name":"s","secret":{"secretName":"allowed"}}]);
    assert_eq!(create(&s, value).await.0, 201);
}

#[tokio::test]
async fn controller_repairs_legacy_namespace_with_empty_namespace_metadata() {
    use h3s_storage::{SqliteStore, Storage, StoreKey, StoredObject};
    let dir = tempfile::tempdir().unwrap();
    // Reproduce persisted metadata emitted by a previous Protobuf API version,
    // before starting the actual API/controller. This is migration fixture data.
    let store = SqliteStore::open(dir.path().join("registry.db"))
        .await
        .unwrap();
    store.create(StoredObject {
        key: StoreKey::new("/registry/namespaces/legacy").unwrap(), revision: 0,
        value: serde_json::to_vec(&json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":"legacy","namespace":"","uid":"12345678-1234-1234-1234-123456789012"},"status":{"phase":"Active"}})).unwrap(),
    }).await.unwrap();
    drop(store);
    let s = Server::start_with_controllers(dir.path()).await;
    let ns = "/api/v1/namespaces/legacy";
    let (_, namespace) = s.json(s.admin(), "GET", ns, json!({})).await;
    assert!(namespace["metadata"].get("namespace").is_none());
    let sa = format!("{ns}/serviceaccounts/default");
    let original = wait_object(&s, &sa, |_| true).await;
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            &sa,
            json!({"preconditions":{"uid":original["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    let repaired = wait_object(&s, &sa, |v| {
        v["metadata"]["uid"] != original["metadata"]["uid"]
    })
    .await;
    assert_ne!(repaired["metadata"]["uid"], original["metadata"]["uid"]);
    let (code,created)=s.json(s.admin(),"POST","/api/v1/namespaces",json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":"empty","namespace":""}})).await;
    assert_eq!(code, 201, "{created}");
    assert!(created["metadata"].get("namespace").is_none());
}
