//! Stable initial identities for handwritten configuration, without read writes.
use sha1::{Digest, Sha1};
use uuid::Uuid;

/// Explicit IDs are immutable. Only absent IDs derive from the object's kind
/// and name, so independent reads agree until a mutation persists the ID.
pub(super) fn normalize(document: &mut toml::Value) {
    for (section, kind) in [("servers", "server"), ("forwards", "forward")] {
        let Some(entries) = document
            .get_mut(section)
            .and_then(toml::Value::as_array_mut)
        else {
            continue;
        };
        for entry in entries {
            let Some(table) = entry.as_table_mut() else {
                continue;
            };
            if table.contains_key("id") {
                continue;
            }
            if let Some(name) = table.get("name").and_then(toml::Value::as_str) {
                let name = format!("fwm:config:identity:v1:{kind}:{name}");
                let id = name_uuid(Uuid::NAMESPACE_URL, &name);
                table.insert("id".into(), toml::Value::String(id.to_string()));
            }
        }
    }
}

/// RFC 9562 UUIDv5, using the SHA-1 implementation already needed for SSH trust.
fn name_uuid(namespace: Uuid, name: &str) -> Uuid {
    let mut digest = Sha1::new();
    digest.update(namespace.as_bytes());
    digest.update(name.as_bytes());
    let hash = digest.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v5_matches_the_standard_dns_vector() {
        assert_eq!(
            name_uuid(Uuid::NAMESPACE_DNS, "www.widgets.com").to_string(),
            "21f7f8de-8051-5b89-8680-0195ef798b6a"
        );
    }

    #[test]
    fn missing_ids_are_stable_and_separate_kinds_while_explicit_ids_stay_unchanged() {
        let source = r#"
[[servers]]
name = "same-name"
[[servers]]
name = "explicit"
id = "user-chosen-id"
[[forwards]]
name = "same-name"
"#;
        let mut first: toml::Value = toml::from_str(source).unwrap();
        let mut second: toml::Value = toml::from_str(source).unwrap();
        normalize(&mut first);
        normalize(&mut second);
        assert_eq!(first, second);
        assert_ne!(first["servers"][0]["id"], first["forwards"][0]["id"]);
        assert_eq!(first["servers"][1]["id"].as_str(), Some("user-chosen-id"));
        assert!(Uuid::parse_str(first["servers"][0]["id"].as_str().unwrap()).is_ok());
        let normalized = first.clone();
        normalize(&mut first);
        assert_eq!(first, normalized);
    }
}
