use super::*;

use std::fs;
use std::path::{Path, PathBuf};

/// A scratch directory removed when the test ends.
struct Scratch {
    /// The scratch directory itself.
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Creates a scratch directory named after the test.
fn scratch(name: &str) -> Scratch {
    let path = std::env::temp_dir().join(format!("leg-install-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch directory");
    Scratch { path }
}

/// Writes a fake native binary and returns the source entry for it.
fn write_source(directory: &Path, file_name: &str, content: &[u8]) -> SourceBinary {
    let path = directory.join(file_name);
    fs::write(&path, content).expect("write source");
    SourceBinary {
        file_name: file_name.to_string(),
        path,
    }
}

/// Builds the four fake companion sources inside `directory`.
fn fake_sources(directory: &Path) -> Vec<SourceBinary> {
    fs::create_dir_all(directory).expect("create bundle directory");
    COMPANION_BINARIES
        .iter()
        .map(|name| {
            let file_name = format!("{name}{}", executable_suffix());
            write_source(directory, &file_name, name.as_bytes())
        })
        .collect()
}

/// The file names the destination holds after an install.
fn destination_entries(destination: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(destination)
        .expect("read destination")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn install_sources_copies_every_companion_into_the_destination() {
    let scratch = scratch("copies");
    let sources = fake_sources(&scratch.path.join("bundle"));
    let destination = scratch.path.join("bin");

    let report = install_sources(
        &sources,
        &InstallOptions {
            destination: Some(destination.clone()),
            no_path: true,
        },
        FaultPoint::None,
    )
    .expect("install succeeds");

    assert_eq!(report.installed.len(), COMPANION_BINARIES.len());
    assert_eq!(report.destination, destination);
    for source in &sources {
        let installed = destination.join(&source.file_name);
        assert!(
            installed.is_file(),
            "{} was not installed",
            source.file_name
        );
        assert_eq!(
            fs::read(&installed).expect("read installed file"),
            fs::read(&source.path).expect("read source file")
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for source in &sources {
            let mode = fs::metadata(destination.join(&source.file_name))
                .expect("installed metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "{} is not executable",
                source.file_name
            );
        }
    }
}

#[test]
fn repeated_installs_update_in_place_without_duplicate_files() {
    let scratch = scratch("repeat");
    let bundle = scratch.path.join("bundle");
    let sources = fake_sources(&bundle);
    let destination = scratch.path.join("bin");
    let options = InstallOptions {
        destination: Some(destination.clone()),
        no_path: true,
    };

    install_sources(&sources, &options, FaultPoint::None).expect("first install succeeds");
    let first = fs::read(destination.join(&sources[0].file_name)).expect("read installed file");

    // The bundle is replaced by a newer build before the second install.
    for source in &sources {
        fs::write(&source.path, b"updated").expect("update source");
    }
    let report =
        install_sources(&sources, &options, FaultPoint::None).expect("second install succeeds");

    assert_eq!(report.installed.len(), COMPANION_BINARIES.len());
    assert_eq!(
        destination_entries(&destination),
        vec![
            format!("leg-tui{}", executable_suffix()),
            format!("leg-ui-supervisor{}", executable_suffix()),
            format!("leg-web{}", executable_suffix()),
            format!("leg{}", executable_suffix()),
        ]
    );
    let second = fs::read(destination.join(&sources[0].file_name)).expect("read installed file");
    assert_ne!(first, second, "the reinstall did not update the file");
    assert_eq!(second, b"updated");
}

#[test]
fn a_staging_failure_leaves_the_destination_untouched() {
    let scratch = scratch("staging-failure");
    let bundle = scratch.path.join("bundle");
    let sources = fake_sources(&bundle);
    let destination = scratch.path.join("bin");
    let options = InstallOptions {
        destination: Some(destination.clone()),
        no_path: true,
    };

    install_sources(&sources, &options, FaultPoint::None).expect("first install succeeds");
    let before = fs::read(destination.join(&sources[0].file_name)).expect("read installed file");

    let error = install_sources(&sources, &options, FaultPoint::Stage).expect_err("staging fails");
    assert!(
        error.to_string().contains("staging"),
        "unexpected error: {error}"
    );

    assert_eq!(
        fs::read(destination.join(&sources[0].file_name)).expect("read installed file"),
        before
    );
    assert_eq!(
        destination_entries(&destination).len(),
        COMPANION_BINARIES.len()
    );
}

#[test]
fn a_commit_failure_restores_the_previous_files() {
    let scratch = scratch("commit-failure");
    let bundle = scratch.path.join("bundle");
    let sources = fake_sources(&bundle);
    let destination = scratch.path.join("bin");
    let options = InstallOptions {
        destination: Some(destination.clone()),
        no_path: true,
    };

    install_sources(&sources, &options, FaultPoint::None).expect("first install succeeds");
    let before = fs::read(destination.join(&sources[0].file_name)).expect("read installed file");

    let error = install_sources(&sources, &options, FaultPoint::Commit).expect_err("commit fails");
    assert!(
        error.to_string().contains("committing"),
        "unexpected error: {error}"
    );

    assert_eq!(
        fs::read(destination.join(&sources[0].file_name)).expect("read installed file"),
        before,
        "the replaced file was not restored"
    );
    assert_eq!(
        destination_entries(&destination),
        vec![
            format!("leg-tui{}", executable_suffix()),
            format!("leg-ui-supervisor{}", executable_suffix()),
            format!("leg-web{}", executable_suffix()),
            format!("leg{}", executable_suffix()),
        ],
        "the failed commit left files or staging directories behind"
    );
}

#[test]
fn missing_companions_are_named_in_the_failure() {
    let scratch = scratch("missing");
    let bundle = scratch.path.join("bundle");
    fs::create_dir_all(&bundle).expect("create bundle directory");
    let running = write_source(&bundle, &format!("leg{}", executable_suffix()), b"leg");

    let error = discover_sources_in(&bundle, &running.path).expect_err("companions are missing");
    let message = error.to_string();
    for name in ["leg-tui", "leg-web", "leg-ui-supervisor"] {
        assert!(
            message.contains(name),
            "the failure did not name {name}: {message}"
        );
    }
}

#[test]
fn script_sources_are_rejected() {
    let scratch = scratch("script");
    let bundle = scratch.path.join("bundle");
    fs::create_dir_all(&bundle).expect("create bundle directory");
    let running = write_source(
        &bundle,
        &format!("leg{}", executable_suffix()),
        b"#!/bin/sh\nexec leg \"$@\"\n",
    );

    let error = discover_sources_in(&bundle, &running.path).expect_err("a shim is rejected");
    assert!(
        error.to_string().contains("script"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_running_executable_stands_in_for_a_missing_sibling() {
    let scratch = scratch("running");
    let bundle = scratch.path.join("bundle");
    fs::create_dir_all(&bundle).expect("create bundle directory");
    let running = write_source(&bundle, &format!("leg{}", executable_suffix()), b"leg");
    for name in ["leg-tui", "leg-web", "leg-ui-supervisor"] {
        write_source(
            &bundle,
            &format!("{name}{}", executable_suffix()),
            name.as_bytes(),
        );
    }

    let sources = discover_sources_in(&bundle, &running.path).expect("the sources are found");
    assert_eq!(sources.len(), COMPANION_BINARIES.len());
    assert_eq!(sources[0].path, running.path);
}

#[test]
fn merge_path_value_appends_a_missing_entry() {
    let merged = merge_path_value(
        "C:\\Windows;C:\\Tools",
        Path::new("C:\\Users\\me\\leg\\bin"),
    );
    assert_eq!(
        merged.value,
        "C:\\Windows;C:\\Tools;C:\\Users\\me\\leg\\bin"
    );
    assert!(!merged.already_present);
    assert!(merged.changed);
}

#[test]
fn merge_path_value_deduplicates_the_entry_case_insensitively() {
    let merged = merge_path_value(
        "C:\\Windows;c:\\users\\me\\leg\\bin;C:\\Tools",
        Path::new("C:\\Users\\Me\\Leg\\Bin"),
    );
    assert_eq!(
        merged.value,
        "C:\\Windows;c:\\users\\me\\leg\\bin;C:\\Tools"
    );
    assert!(merged.already_present);
    assert!(!merged.changed);
}

#[test]
fn merge_path_value_drops_a_repeated_entry() {
    let merged = merge_path_value(
        "C:\\Windows;C:\\Users\\me\\leg\\bin;C:\\Users\\Me\\Leg\\Bin",
        Path::new("C:\\Users\\me\\leg\\bin"),
    );
    assert_eq!(merged.value, "C:\\Windows;C:\\Users\\me\\leg\\bin");
    assert!(merged.already_present);
    assert!(merged.changed);
}

#[test]
fn merge_path_value_ignores_a_trailing_separator() {
    let merged = merge_path_value(
        "C:\\Windows;C:\\Users\\me\\leg\\bin\\",
        Path::new("C:\\Users\\me\\leg\\bin"),
    );
    assert!(merged.already_present);
    assert!(merged.value.contains("C:\\Users\\me\\leg\\bin"));
}

#[test]
fn merge_path_value_keeps_an_unchanged_value_untouched() {
    let merged = merge_path_value("C:\\Windows", Path::new("C:\\Windows"));
    assert_eq!(merged.value, "C:\\Windows");
    assert!(merged.already_present);
    assert!(!merged.changed);
}

#[test]
fn merge_path_value_preserves_unrelated_entries() {
    let merged = merge_path_value(
        "C:\\Windows;C:\\Program Files\\Git\\cmd",
        Path::new("C:\\Users\\me\\leg\\bin"),
    );
    assert!(merged.value.contains("C:\\Program Files\\Git\\cmd"));
    assert!(merged.value.contains("C:\\Windows"));
}

#[test]
fn default_destination_follows_the_platform_layout() {
    #[cfg(windows)]
    {
        if std::env::var_os("LOCALAPPDATA").is_none() {
            return;
        }
        let destination = default_destination().expect("a default destination");
        assert!(destination.ends_with("leg/bin"), "{destination:?}");
    }
    #[cfg(not(windows))]
    {
        if std::env::var_os("XDG_BIN_HOME").is_none() && std::env::var_os("HOME").is_none() {
            return;
        }
        let destination = default_destination().expect("a default destination");
        if std::env::var_os("XDG_BIN_HOME").is_none() {
            assert!(destination.ends_with(".local/bin"), "{destination:?}");
        }
    }
}

#[cfg(windows)]
#[test]
fn the_registry_backend_round_trips_a_scratch_key() {
    let key = format!("Software\\leg-install-tests\\{}", std::process::id());
    super::windows_path::create_key(&key).expect("create the scratch key");
    super::windows_path::write_path_at(&key, "C:\\one;C:\\two").expect("write the scratch path");
    let value = super::windows_path::read_path_at(&key).expect("read the scratch path");
    assert_eq!(value, "C:\\one;C:\\two");
}

#[cfg(windows)]
#[test]
fn the_registry_backend_reads_the_current_user_path() {
    // The current user's PATH is read-only here; the install command is what
    // writes it.
    let value = super::windows_path::read_user_path().expect("read the current user's PATH");
    assert!(!value.contains('\0'));
}
