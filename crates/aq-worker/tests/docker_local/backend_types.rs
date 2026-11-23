//! Backend type contracts and serialized values.

use aq_worker::backend::*;
use uuid::Uuid;

fn request() -> CreateRequest {
    CreateRequest::new(
        Ownership {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(3),
        },
        TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
        100,
        vec![],
    )
}

#[test]
fn create_request_without_resources_deserializes() {
    let mut value = serde_json::to_value(request()).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("resources");
    object.remove("mount_aliases");
    let parsed: CreateRequest = serde_json::from_value(value).unwrap();
    assert_eq!(parsed.resources, None);
    assert!(parsed.mount_aliases.is_empty());
    assert_eq!(parsed, request());
}

#[test]
fn absent_disk_limit_keeps_pre_sbx20_encoding() {
    let limits = ResourceLimits {
        cpu_millis: 500,
        memory_bytes: 64 * 1024 * 1024,
        pids: 32,
        disk_bytes: None,
    };
    let value = serde_json::to_value(limits).unwrap();
    assert!(value.get("disk_bytes").is_none());
    let parsed: ResourceLimits = serde_json::from_value(
        serde_json::json!({"cpu_millis":500,"memory_bytes":67108864,"pids":32}),
    )
    .unwrap();
    assert_eq!(parsed, limits);
}

#[test]
fn resource_limits_reject_zero() {
    let ok = ResourceLimits {
        cpu_millis: 500,
        memory_bytes: 64 * 1024 * 1024,
        pids: 32,
        disk_bytes: None,
    };
    assert!(request().with_resources(ok).validate().is_ok());
    let with_disk = ResourceLimits {
        disk_bytes: Some(1),
        ..ok
    };
    assert!(request().with_resources(with_disk).validate().is_ok());
    for bad in [
        ResourceLimits {
            cpu_millis: 0,
            ..ok
        },
        ResourceLimits { pids: 0, ..ok },
        ResourceLimits {
            memory_bytes: 6 * 1024 * 1024 - 1,
            ..ok
        },
        ResourceLimits {
            disk_bytes: Some(0),
            ..ok
        },
    ] {
        assert!(matches!(
            request().with_resources(bad).validate(),
            Err(BackendError::InvalidRequest(_))
        ));
    }
}

#[test]
fn mount_alias_names_are_validated() {
    for bad in ["", "../x", "a/b", "-x", "Upper"] {
        assert!(
            matches!(
                request()
                    .with_mount_aliases(vec![bad.to_owned()])
                    .validate(),
                Err(BackendError::InvalidRequest(_))
            ),
            "{bad:?} must be refused"
        );
    }
    assert!(
        request()
            .with_mount_aliases(vec!["repo-cache".to_owned()])
            .validate()
            .is_ok()
    );
}
