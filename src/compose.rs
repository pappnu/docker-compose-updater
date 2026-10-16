use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use yaml_serde::Value;

pub fn backup_path(compose_path: &Path) -> PathBuf {
    let mut path: OsString = compose_path.as_os_str().to_owned();
    path.push(".bak");
    PathBuf::from(path)
}

pub fn backup_matches(compose_path: &Path, backup_path: &Path) -> Result<bool, String> {
    let current = read_document(compose_path)?;
    let backup = read_document(backup_path)?;
    backup_matches_documents(&current, &backup)
}

pub fn restore_backup(compose_path: &Path, backup_path: &Path) -> Result<(), String> {
    let contents = fs::read(backup_path)
        .map_err(|error| format!("Could not read backup {}: {error}", backup_path.display()))?;
    write_atomic(compose_path, &contents).map_err(|error| {
        format!(
            "Could not restore backup to {}: {error}",
            compose_path.display()
        )
    })
}

pub fn service_base_images(path: &Path) -> Result<BTreeMap<String, Option<String>>, String> {
    let document = read_document(path)?;
    service_base_images_from_document(&document)
}

pub fn backup_current(compose_path: &Path) -> Result<(), String> {
    let contents = fs::read(compose_path).map_err(|error| {
        format!(
            "Could not read compose file {}: {error}",
            compose_path.display()
        )
    })?;
    let backup = backup_path(compose_path);
    write_atomic(&backup, &contents).map_err(|error| {
        format!(
            "Could not write compose backup {}: {error}",
            backup.display()
        )
    })
}

pub fn apply_image_overrides(
    compose_path: &Path,
    versions: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut document = read_document(compose_path)?;
    let root = document
        .as_mapping_mut()
        .ok_or_else(|| "Compose document must be a YAML mapping".to_owned())?;
    let services = root
        .get_mut("services")
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| "Compose document must define a services mapping".to_owned())?;

    for (service, image) in versions {
        let definition = services
            .get_mut(service.as_str())
            .ok_or_else(|| format!("Snapshot contains unknown compose service '{service}'"))?;
        let definition = definition
            .as_mapping_mut()
            .ok_or_else(|| format!("Compose service '{service}' must be a mapping"))?;
        let image_value = definition
            .get_mut("image")
            .ok_or_else(|| format!("Compose service '{service}' has no image to roll back"))?;
        *image_value = Value::String(image.clone());
    }

    let serialized = yaml_serde::to_string(&document)
        .map_err(|error| format!("Could not serialize compose YAML: {error}"))?;
    write_atomic(compose_path, serialized.as_bytes()).map_err(|error| {
        format!(
            "Could not write compose file {}: {error}",
            compose_path.display()
        )
    })
}

fn read_document(path: &Path) -> Result<Value, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read compose file {}: {error}", path.display()))?;
    yaml_serde::from_str(&contents)
        .map_err(|error| format!("Could not parse compose YAML {}: {error}", path.display()))
}

fn backup_matches_documents(current: &Value, backup: &Value) -> Result<bool, String> {
    Ok(service_base_images_from_document(current)? == service_base_images_from_document(backup)?)
}

fn service_base_images_from_document(
    document: &Value,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let services = document
        .get("services")
        .and_then(Value::as_mapping)
        .ok_or_else(|| "Compose document must define a services mapping".to_owned())?;
    let mut bases = BTreeMap::new();

    for (key, service) in services.iter() {
        let Some(name) = key.as_str() else {
            return Err("Compose service names must be strings".to_owned());
        };
        let definition = service
            .as_mapping()
            .ok_or_else(|| format!("Compose service '{name}' must be a mapping"))?;
        let image = definition.get("image").and_then(Value::as_str);
        bases.insert(name.to_owned(), image.map(base_image_reference));
    }

    Ok(bases)
}

fn base_image_reference(image: &str) -> String {
    let without_digest = image.split_once('@').map_or(image, |(name, _)| name);
    if let Some(colon) = without_digest.rfind(':') {
        let last_slash = without_digest.rfind('/').unwrap_or(0);
        if colon > last_slash {
            return without_digest[..colon].to_owned();
        }
    }
    without_digest.to_owned()
}

fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no file name"))?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".tmp-{}", std::process::id()));
    let temporary_path = parent.join(temporary_name);

    let result = (|| {
        let mut file = fs::File::create(&temporary_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        apply_image_overrides, backup_matches_documents, base_image_reference, read_document,
    };
    use yaml_serde::Value;

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn temporary_file() -> PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "compose-updater-test-{}-{id}.yaml",
            std::process::id()
        ))
    }

    #[test]
    fn running_image_reference_keeps_registry_port_and_tag() {
        assert_eq!(
            base_image_reference("registry.example:5000/team/api:1.2.3"),
            "registry.example:5000/team/api"
        );
        assert_eq!(
            base_image_reference("registry.example:5000/team/api@sha256:abcdef"),
            "registry.example:5000/team/api"
        );
    }

    #[test]
    fn noted_versions_override_compose_images() {
        let path = temporary_file();
        fs::write(
            &path,
            "services:\n  web:\n    image: registry.example:5000/team/web:new\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        let versions = BTreeMap::from([
            (
                "web".to_owned(),
                "registry.example:5000/team/web:old".to_owned(),
            ),
            ("db".to_owned(), "postgres:15".to_owned()),
        ]);

        apply_image_overrides(&path, &versions).unwrap();
        let document = read_document(&path).unwrap();
        assert_eq!(
            document["services"]["web"]["image"].as_str(),
            Some("registry.example:5000/team/web:old")
        );
        assert_eq!(
            document["services"]["db"]["image"].as_str(),
            Some("postgres:15")
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn backup_is_rejected_when_base_services_differ() {
        let current: Value = yaml_serde::from_str(
            "services:\n  web:\n    image: nginx:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        let backup: Value = yaml_serde::from_str(
            "services:\n  web:\n    image: nginx:1\n  cache:\n    image: redis:7\n",
        )
        .unwrap();

        assert!(!backup_matches_documents(&current, &backup).unwrap());
    }

    #[test]
    fn backup_is_approved_when_only_tags_differ() {
        let current: Value = yaml_serde::from_str(
            "services:\n  web:\n    image: nginx:1\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        let backup: Value = yaml_serde::from_str(
            "services:\n  web:\n    image: nginx:latest\n  db:\n    image: postgres:15\n",
        )
        .unwrap();

        assert!(backup_matches_documents(&current, &backup).unwrap());
    }
}
