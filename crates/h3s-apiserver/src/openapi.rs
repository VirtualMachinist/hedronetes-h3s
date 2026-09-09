//! Minimal OpenAPI v2 so stock Helm can validate ConfigMap/Secret/Namespace.
use axum::{
    body::Body,
    http::{
        header::{CONTENT_TYPE, VARY},
        HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

const PROTOBUF: &str = "C\
gMyLjASDgoDaDNzEgd2MS4zNC4wQgBK0AoK3AIKHGlvLms4cy5hcGkuY29yZS52MS5Db25maWdNYXASuwIiCUNvbmZpZ01hcLIBCAoGb2JqZWN0ygHNAQoZCgphcGlWZXJzaW9uEguyAQgKBnN0cmluZwopCgpiaW5hcnlEYXRhEhuqAQ0KC7IBCAoGc3RyaW5nsgEICgZvYmplY3QKIwoEZGF0YRIbqgENCguyAQgKBnN0cmluZ7IBCAoGb2JqZWN0ChMKBGtpbmQSC7IBCAoGc3RyaW5nCksKCG1ldGFkYXRhEj8KPSMvZGVmaW5pdGlvbnMvaW8uazhzLmFwaW1hY2hpbmVyeS5wa2cuYXBpcy5tZXRhLnYxLk9iamVjdE1ldGH6AVEKH3gta3ViZXJuZXRlcy1ncm91cC12ZXJzaW9uLWtpbmQSLhIsLSBncm91cDogIiIKICBraW5kOiBDb25maWdNYXAKICB2ZXJzaW9uOiB2MQoK0QIKHGlvLms4cy5hcGkuY29yZS52MS5OYW1lc3BhY2USsAIiCU5hbWVzcGFjZbIBCAoGb2JqZWN0ygHCAQoZCgphcGlWZXJzaW9uEguyAQgKBnN0cmluZwoTCgRraW5kEguyAQgKBnN0cmluZwpLCghtZXRhZGF0YRI/Cj0jL2RlZmluaXRpb25zL2lvLms4cy5hcGltYWNoaW5lcnkucGtnLmFwaXMubWV0YS52MS5PYmplY3RNZXRhChMKBHNwZWMSC7IBCAoGb2JqZWN0Ci4KBnN0YXR1cxIksgEICgZvYmplY3TKARYKFAoFcGhhc2USC7IBCAoGc3RyaW5n+gFRCh94LWt1YmVybmV0ZXMtZ3JvdXAtdmVyc2lvbi1raW5kEi4SLC0gZ3JvdXA6ICIiCiAga2luZDogTmFtZXNwYWNlCiAgdmVyc2lvbjogdjEKCugCChlpby5rOHMuYXBpLmNvcmUudjEuU2VjcmV0EsoCIgZTZWNyZXSyAQgKBm9iamVjdMoB4gEKGQoKYXBpVmVyc2lvbhILsgEICgZzdHJpbmcKIwoEZGF0YRIbqgENCguyAQgKBnN0cmluZ7IBCAoGb2JqZWN0ChMKBGtpbmQSC7IBCAoGc3RyaW5nCksKCG1ldGFkYXRhEj8KPSMvZGVmaW5pdGlvbnMvaW8uazhzLmFwaW1hY2hpbmVyeS5wa2cuYXBpcy5tZXRhLnYxLk9iamVjdE1ldGEKKQoKc3RyaW5nRGF0YRIbqgENCguyAQgKBnN0cmluZ7IBCAoGb2JqZWN0ChMKBHR5cGUSC7IBCAoGc3RyaW5n+gFOCh94LWt1YmVybmV0ZXMtZ3JvdXAtdmVyc2lvbi1raW5kEisSKS0gZ3JvdXA6ICIiCiAga2luZDogU2VjcmV0CiAgdmVyc2lvbjogdjEKCq8CCi9pby5rOHMuYXBpbWFjaGluZXJ5LnBrZy5hcGlzLm1ldGEudjEuT2JqZWN0TWV0YRL7ASIKT2JqZWN0TWV0YbIBCAoGb2JqZWN0ygHgAQoqCgthbm5vdGF0aW9ucxIbqgENCguyAQgKBnN0cmluZ7IBCAoGb2JqZWN0CigKCmZpbmFsaXplcnMSGrIBBwoFYXJyYXm6AQ0KC7IBCAoGc3RyaW5nCiUKBmxhYmVscxIbqgENCguyAQgKBnN0cmluZ7IBCAoGb2JqZWN0ChMKBG5hbWUSC7IBCAoGc3RyaW5nChgKCW5hbWVzcGFjZRILsgEICgZzdHJpbmcKHgoPcmVzb3VyY2VWZXJzaW9uEguyAQgKBnN0cmluZwoSCgN1aWQSC7IBCAoGc3RyaW5n";
const PROTOBUF_DEPRECATED: &str = "application/com.github.proto-openapi.spec.v2@v1.0+protobuf";
const PROTOBUF_MEDIA_TYPE: &str = "application/com.github.proto-openapi.spec.v2.v1.0+protobuf";

fn protobuf_content_type(accept: &str) -> Option<&'static str> {
    accept.split(',').find_map(|part| {
        let value = part.split(';').next().unwrap_or("").trim();
        if value == PROTOBUF_DEPRECATED {
            Some(PROTOBUF_DEPRECATED)
        } else if value == PROTOBUF_MEDIA_TYPE
            || (value.contains("proto-openapi.spec.v2") && value.contains("protobuf"))
        {
            Some(PROTOBUF_MEDIA_TYPE)
        } else {
            None
        }
    })
}

pub fn v2(accept: Option<&HeaderValue>) -> Response {
    let accept = accept
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*/*");
    if let Some(content_type) = protobuf_content_type(accept) {
        let body = STANDARD
            .decode(PROTOBUF)
            .expect("embedded OpenAPI protobuf must be valid base64");
        return (
            [(CONTENT_TYPE, content_type), (VARY, "Accept")],
            Body::from(body),
        )
            .into_response();
    }
    if !accept
        .split(',')
        .map(|value| value.split(';').next().unwrap_or("").trim())
        .any(|value| matches!(value, "*/*" | "application/*" | "application/json"))
    {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let mut response = Json(document()).into_response();
    response
        .headers_mut()
        .insert(VARY, HeaderValue::from_static("Accept"));
    response
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
