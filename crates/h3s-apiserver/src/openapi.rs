//! Minimal OpenAPI v2 so stock Helm can validate ConfigMap/Secret/Namespace.
use axum::{response::IntoResponse, Json};
use serde_json::{json, Value};

pub fn v2() -> axum::response::Response {
    Json(document()).into_response()
}

fn gvk(kind: &str) -> Value {
    json!([{"group":"","kind":kind,"version":"v1"}])
}

fn object_meta() -> Value {
    json!({
        "description": "ObjectMeta",
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "namespace": {"type": "string"},
            "uid": {"type": "string"},
            "resourceVersion": {"type": "string"},
            "labels": {"type": "object", "additionalProperties": {"type": "string"}},
            "annotations": {"type": "object", "additionalProperties": {"type": "string"}},
            "finalizers": {"type": "array", "items": {"type": "string"}}
        }
    })
}

fn core_object(kind: &str, extra: Value) -> Value {
    let mut properties = json!({
        "apiVersion": {"type": "string"},
        "kind": {"type": "string"},
        "metadata": {"$ref": "#/definitions/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta"}
    });
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            properties[k] = v.clone();
        }
    }
    json!({
        "description": kind,
        "type": "object",
        "x-kubernetes-group-version-kind": gvk(kind),
        "properties": properties
    })
}

pub fn document() -> Value {
    json!({
        "swagger": "2.0",
        "info": {"title": "h3s", "version": "v1.34.0"},
        "paths": {},
        "definitions": {
            "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta": object_meta(),
            "io.k8s.api.core.v1.ConfigMap": core_object("ConfigMap", json!({
                "data": {"type": "object", "additionalProperties": {"type": "string"}},
                "binaryData": {"type": "object", "additionalProperties": {"type": "string"}}
            })),
            "io.k8s.api.core.v1.Secret": core_object("Secret", json!({
                "type": {"type": "string"},
                "data": {"type": "object", "additionalProperties": {"type": "string"}},
                "stringData": {"type": "object", "additionalProperties": {"type": "string"}}
            })),
            "io.k8s.api.core.v1.Namespace": core_object("Namespace", json!({
                "spec": {"type": "object"},
                "status": {"type": "object", "properties": {"phase": {"type": "string"}}}
            }))
        }
    })
}
