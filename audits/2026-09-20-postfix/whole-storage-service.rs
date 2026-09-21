//! Pure definition/adapter discovery audit; no native service manager calls.
use super::*;

#[test]
fn whole_linux_definition_roundtrip_matrix() {
    for segment in [
        "plain".to_string(), "with space".into(), "unicode-配置".into(),
        "dollar-$-percent-%".into(), "a\\b".into(), "a\"b".into(),
        format!("b{}\"q", "\\".repeat(1)),
        format!("b{}\"q", "\\".repeat(2)),
        format!("b{}\"q", "\\".repeat(3)),
        format!("b{}\"q", "\\".repeat(4)),
    ] {
        let mut f = Fixture::new(Platform::Linux);
        f.paths = Paths::new(Some(f.directory.path().join(&segment))).unwrap();
        f.paths.ensure_dirs().unwrap();
        let adapter = f.adapter("/usr/local/bin/fwm");
        let body = adapter.render().unwrap();
        let recovered = discovery::unit_profile(&body).unwrap();
        let slash_count = segment.chars().filter(|c| *c == '\\').count();
        let affected = segment.ends_with("\"q") && slash_count >= 2;
        assert_eq!(recovered != f.paths.config_dir, affected);
        println!("ROUNDTRIP linux segment={segment:?} expected={:?} decoded={recovered:?} mismatch={affected}", f.paths.config_dir);
    }
}

#[test]
fn whole_linux_legacy_definition_discovery_misses_a_legitimate_escaped_path() {
    for slashes in [1, 2, 3] {
        let mut f = Fixture::new(Platform::Linux);
        let segment = format!("profile-{}\"name", "\\".repeat(slashes));
        f.paths = Paths::new(Some(f.directory.path().join(segment))).unwrap();
        f.paths.ensure_dirs().unwrap();
        let adapter = f.adapter("/usr/local/bin/fwm");
        let body = adapter.render().unwrap();
        fs::create_dir_all(f.definition_dir()).unwrap();
        // A supported older service identifier, with a valid definition rendered
        // by the current adapter. A stopped unit need not appear in list-units.
        let legacy = f.definition_dir().join("fwm-1000000000000001.service");
        fs::write(&legacy, body).unwrap();
        let discovered = f.discovered("/usr/local/bin/fwm");
        if slashes == 1 {
            assert_eq!(discovered.definition_path(), legacy);
            assert!(discovered.is_installed());
        } else {
            assert_ne!(discovered.definition_path(), legacy);
            assert!(!discovered.is_installed());
            assert!(legacy.is_file());
            let mut operation = f.operation("/usr/local/bin/fwm");
            assert!(!operation.installed().unwrap());
            drop(operation);
        }
        assert!(f.machine.lock().unwrap().tasks.is_empty());
        println!("LEGACY-DISCOVERY linux adjacent_backslashes={slashes} found_existing_definition={} manager_mutations=0", discovered.is_installed());
        // The current identifier's filename fallback still recognizes the same
        // path. The regression is the older identifier discovery path.
        fs::write(adapter.definition_path(), adapter.render().unwrap()).unwrap();
        let canonical = f.discovered("/usr/local/bin/fwm");
        assert!(canonical.is_installed());
        println!("CONTROL linux adjacent_backslashes={slashes} canonical_filename_found=true");
    }
}

#[test]
fn whole_windows_and_macos_definition_paths_roundtrip() {
    for value in [r"C:\Users\Test User\fwm", r"C:\config\", r"\\server\share\folder\", r"C:\Percent%$\配置"] {
        let f = Fixture::new(Platform::Macos);
        let mut paths = f.paths.clone();
        // PathBuf is just a carrier here. No Windows FS/canonicalization calls.
        paths.config_dir = PathBuf::from(value);
        let adapter = windows::Service::new(&paths, "S-1-5-21-1".into(), r"C:\Program Files\fwm.exe".into());
        let rendered = adapter.render().unwrap();
        assert_eq!(discovery::task_profile(&rendered), Some(PathBuf::from(value)));
    }
    for segment in ["plain", "space and 配置", "&<>\"'", "a\\\\\"b"] {
        let mut f = Fixture::new(Platform::Macos);
        f.paths = Paths::new(Some(f.directory.path().join(segment))).unwrap();
        let rendered = f.adapter("/usr/local/bin/fwm").render().unwrap();
        assert_eq!(discovery::plist_profile(&rendered), Some(f.paths.config_dir.clone()));
    }
    println!("CONTROL Windows trailing-separator/UNC/space/Unicode arguments and macOS XML values round-trip");
}
