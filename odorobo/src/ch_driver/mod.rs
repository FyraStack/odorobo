//! Cloud Hypervisor-backed VM runtime.
//!
//! The agent supplies provider-neutral manifests; this module owns conversion
//! into Cloud Hypervisor configuration and all provider-specific runtime work.

pub mod actor;
pub mod api;
pub mod devices;
pub mod instance;
pub mod manifest;
pub mod provisioning;
pub mod transform;

pub use instance::VMInstance;

#[cfg(test)]
mod tests {
    use cloud_hypervisor_client::models::VmConfig;
    use serde_json::Value;

    use super::manifest::to_vm_config;
    use super::transform::{ConfigTransform, ConsoleTransform};
    use crate::manifest::VmManifest;

    const API_SPEC: &str = include_str!("../../../docs/schemas/cloud-hypervisor.json");

    const MANIFEST_FIXTURES: &[&str] = &[
        include_str!("../../../docs/fixtures/manifest/minimal.json"),
        include_str!("../../../docs/fixtures/manifest/networked.json"),
        include_str!("../../../docs/fixtures/manifest/vsock.json"),
    ];

    fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
        schema
            .get("$ref")
            .and_then(Value::as_str)
            .map_or(schema, |reference| {
                spec.pointer(reference.trim_start_matches('#'))
                    .expect("reference resolves against the spec")
            })
    }

    fn collect_spec_problems(
        spec: &Value,
        schema: &Value,
        value: &Value,
        path: &str,
        problems: &mut Vec<String>,
    ) {
        let schema = resolve(spec, schema);

        if let (Some(Value::Object(properties)), Value::Object(map)) =
            (schema.get("properties"), value)
        {
            for (key, item) in map {
                match properties.get(key) {
                    Some(property) => {
                        collect_spec_problems(
                            spec,
                            property,
                            item,
                            &format!("{path}.{key}"),
                            problems,
                        );
                    }
                    None => {
                        problems.push(format!("{path}.{key} is not in the Cloud Hypervisor spec"))
                    }
                }
            }

            for required in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = required.as_str() {
                    if !map.contains_key(name) {
                        problems.push(format!("{path}.{name} is required by the spec"));
                    }
                }
            }
        }

        if let (Some(items), Value::Array(entries)) = (schema.get("items"), value) {
            for (index, entry) in entries.iter().enumerate() {
                collect_spec_problems(spec, items, entry, &format!("{path}[{index}]"), problems);
            }
        }
    }

    fn generated_config(json: &str) -> VmConfig {
        let manifest: VmManifest = serde_json::from_str(json).expect("fixture is a valid manifest");
        let mut config = to_vm_config(&manifest).expect("manifest converts to a VM config");
        ConsoleTransform
            .transform("test-vm", &mut config)
            .expect("console transform applies");
        config
    }

    #[test]
    fn generated_configs_match_cloud_hypervisor_spec() {
        let spec: Value = serde_json::from_str(API_SPEC).expect("api spec is valid JSON");
        let ref_schema = serde_json::json!({"$ref": "#/components/schemas/VmConfig"});
        let root = resolve(&spec, &ref_schema);

        for json in MANIFEST_FIXTURES {
            let config = serde_json::to_value(generated_config(json)).expect("config serializes");
            let mut problems = Vec::new();
            collect_spec_problems(&spec, root, &config, "VmConfig", &mut problems);
            assert!(problems.is_empty(), "{problems:#?}");
        }
    }
}
