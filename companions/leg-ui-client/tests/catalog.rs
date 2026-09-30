use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use leg_ui_client::{
    CatalogError, CatalogRunState, SessionCatalog, SessionCatalogConfig, SessionInterface,
};
use serde_json::json;
use tempfile::tempdir;

fn config(state_dir: &Path) -> SessionCatalogConfig {
    SessionCatalogConfig {
        state_dir: Some(state_dir.to_path_buf()),
        ..SessionCatalogConfig::default()
    }
}

fn request_event(session_id: &str, turn_index: u64, prompt: &str) -> String {
    serde_json::to_string(&json!({
        "schema": "baton.exchange/v1",
        "event": "request",
        "ts_ms": 1,
        "model": "fixture",
        "base_url": "http://fixture.invalid",
        "prompt": prompt,
        "session_id": session_id,
        "turn_index": turn_index
    }))
    .unwrap()
        + "\n"
}

#[test]
fn catalog_locations_recovery_and_metadata_are_safe() {
    let scratch = tempdir().unwrap();
    let state = scratch.path().join("state");
    let cwd = scratch.path().join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let catalog = SessionCatalog::open(config(&state)).unwrap();
    assert_eq!(catalog.sessions_dir(), state.join("sessions"));

    let draft = catalog
        .create_draft(SessionInterface::Tui, Some("work".into()), Some(&cwd))
        .unwrap();
    catalog
        .save_draft(&draft.id, SessionInterface::Tui, "tui text".into())
        .unwrap();
    catalog
        .save_draft(&draft.id, SessionInterface::Web, "web text".into())
        .unwrap();
    catalog
        .save_display_metadata(&draft.id, "theme".into(), json!({"color":"blue"}))
        .unwrap();

    let reopened = SessionCatalog::open(config(&state)).unwrap();
    let persisted = reopened.get(&draft.id).unwrap();
    assert_eq!(persisted.drafts[&SessionInterface::Tui], "tui text");
    assert_eq!(persisted.drafts[&SessionInterface::Web], "web text");
    assert_eq!(persisted.display["theme"], json!({"color":"blue"}));
    assert_eq!(persisted.run_state, CatalogRunState::Idle);

    let orphan_id = "sess-101-202";
    fs::write(
        reopened.sessions_dir().join(format!("{orphan_id}.jsonl")),
        request_event(orphan_id, 0, "incomplete prompt"),
    )
    .unwrap();
    fs::write(
        reopened
            .sessions_dir()
            .join(".leg-ui-session-lock-is-not-a-trail.lock"),
        "",
    )
    .unwrap();
    fs::write(
        reopened.sessions_dir().join("malformed.jsonl"),
        "not json\n",
    )
    .unwrap();
    let listed = reopened.list().unwrap();
    assert_eq!(listed.len(), 3, "lock files are not discovered as sessions");
    let recovered = listed.iter().find(|entry| entry.id == orphan_id).unwrap();
    assert!(recovered.recovered);
    assert!(recovered.read_only);
    assert_eq!(
        recovered.turns[0].outcome,
        leg_ui_client::TrailOutcome::Incomplete
    );
    assert!(
        listed
            .iter()
            .find(|entry| entry.id == "malformed")
            .unwrap()
            .read_only
    );

    reopened.set_workspace(orphan_id, &cwd).unwrap();
    let assigned = reopened.get(orphan_id).unwrap();
    assert!(!assigned.recovered);
    assert!(!assigned.read_only);
    assert_eq!(assigned.cwd.as_deref(), Some(cwd.as_path()));

    assert!(matches!(
        reopened.get("../outside"),
        Err(CatalogError::InvalidSessionId)
    ));
    assert!(matches!(
        reopened.get("sess-999-999"),
        Err(CatalogError::NotFound(_))
    ));

    let partial_temp = state.join(".catalog.json.tmp-interrupted-write");
    fs::write(&partial_temp, b"{\"version\":").unwrap();
    let after_restart = SessionCatalog::open(config(&state)).unwrap();
    assert!(after_restart.get(&draft.id).is_ok());
    assert!(partial_temp.exists(), "incomplete temp files are ignored");
}

#[cfg(unix)]
#[test]
fn catalog_does_not_follow_trail_symlinks_outside_managed_store() {
    use std::os::unix::fs::symlink;

    let scratch = tempdir().unwrap();
    let state = scratch.path().join("state");
    let outside = scratch.path().join("outside.jsonl");
    let catalog = SessionCatalog::open(config(&state)).unwrap();
    fs::write(
        &outside,
        request_event("sess-505-606", 0, "outside workspace"),
    )
    .unwrap();
    symlink(&outside, catalog.sessions_dir().join("sess-505-606.jsonl")).unwrap();

    assert!(matches!(
        catalog.get("sess-505-606"),
        Err(CatalogError::NotFound(_))
    ));
    assert!(catalog.list().unwrap().is_empty());
}

#[test]
fn active_driver_lock_blocks_workspace_change() {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let scratch = tempdir().unwrap();
    let state = scratch.path().join("state");
    let cwd_a = scratch.path().join("a");
    let cwd_b = scratch.path().join("b");
    fs::create_dir_all(&cwd_a).unwrap();
    fs::create_dir_all(&cwd_b).unwrap();
    let catalog = SessionCatalog::open(config(&state)).unwrap();
    let id = "sess-303-404";
    fs::write(
        catalog.sessions_dir().join(format!("{id}.jsonl")),
        request_event(id, 0, "a prompt"),
    )
    .unwrap();
    catalog.set_workspace(id, &cwd_a).unwrap();

    let lock_path = catalog
        .sessions_dir()
        .join(format!(".leg-ui-session-{id}.lock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(matches!(
        catalog.set_workspace(id, &cwd_b),
        Err(CatalogError::Busy)
    ));
    assert!(matches!(catalog.prepare_retry(id), Err(CatalogError::Busy)));
    assert_eq!(catalog.get(id).unwrap().run_state, CatalogRunState::Active);
    FileExt::unlock(&lock).unwrap();
    assert!(catalog.prepare_retry(id).is_ok());
    catalog.set_workspace(id, &cwd_b).unwrap();
    assert_eq!(
        catalog.get(id).unwrap().cwd.as_deref(),
        Some(cwd_b.as_path())
    );
}

#[test]
fn missing_workspace_blocks_submission_until_explicit_replacement() {
    let scratch = tempdir().unwrap();
    let state = scratch.path().join("state");
    let original = scratch.path().join("original");
    let replacement = scratch.path().join("replacement");
    fs::create_dir_all(&original).unwrap();
    fs::create_dir_all(&replacement).unwrap();
    let catalog = SessionCatalog::open(config(&state)).unwrap();
    let draft = catalog
        .create_draft(SessionInterface::Web, Some("draft".into()), Some(&original))
        .unwrap();
    fs::remove_dir(&original).unwrap();

    assert!(matches!(
        catalog.start_new(&draft.id, SessionInterface::Web, "must not submit"),
        Err(CatalogError::WorkspaceMissing(_))
    ));
    catalog.set_workspace(&draft.id, &replacement).unwrap();
    assert_eq!(
        catalog.get(&draft.id).unwrap().cwd.as_deref(),
        Some(replacement.as_path())
    );
}

#[test]
fn catalog_process_worker() {
    let Ok(role) = std::env::var("LEG_UI_CATALOG_PROCESS_WORKER") else {
        return;
    };
    let state = PathBuf::from(
        std::env::var_os("LEG_UI_CATALOG_STATE_DIR").expect("worker state directory"),
    );
    let catalog = SessionCatalog::open(config(&state)).unwrap();
    for index in 0..8 {
        let draft = catalog
            .create_draft(SessionInterface::Tui, Some(format!("{role}-{index}")), None)
            .unwrap();
        catalog
            .save_draft(
                &draft.id,
                SessionInterface::Tui,
                format!("draft-{role}-{index}"),
            )
            .unwrap();
        catalog
            .save_display_metadata(&draft.id, "worker".into(), json!(role))
            .unwrap();
        catalog
            .rename(&draft.id, Some(format!("{role}-renamed-{index}")))
            .unwrap();
    }
}

fn wait_success(children: Vec<Child>) {
    for mut child in children {
        let output = child.wait().expect("wait for catalog process");
        assert!(output.success(), "catalog worker failed: {output}");
    }
}

#[test]
fn concurrent_process_updates_do_not_lose_catalog_records() {
    let scratch = tempdir().unwrap();
    let state = scratch.path().join("state");
    let worker = std::env::current_exe().unwrap();
    let sentinel = "catalog-secret-sentinel-4815";
    let children = ["tui", "web"]
        .into_iter()
        .map(|role| {
            Command::new(&worker)
                .args(["--exact", "catalog_process_worker"])
                .env("LEG_UI_CATALOG_PROCESS_WORKER", role)
                .env("LEG_UI_CATALOG_STATE_DIR", &state)
                .env("ANTHROPIC_API_KEY", sentinel)
                .spawn()
                .expect("spawn catalog process")
        })
        .collect::<Vec<_>>();
    wait_success(children);

    let catalog = SessionCatalog::open(config(&state)).unwrap();
    let entries = catalog.list().unwrap();
    assert_eq!(entries.len(), 16);
    for role in ["tui", "web"] {
        for index in 0..8 {
            assert!(
                entries.iter().any(|entry| {
                    entry.name.as_deref() == Some(format!("{role}-renamed-{index}").as_str())
                        && entry.drafts[&SessionInterface::Tui] == format!("draft-{role}-{index}")
                        && entry.display["worker"] == json!(role)
                }),
                "lost {role} record {index}"
            );
        }
    }
    let index = fs::read_to_string(state.join("catalog.json")).unwrap();
    assert!(!index.contains(sentinel));
}
