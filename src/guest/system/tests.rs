//! Backend tests for the apk driver.
//!
//! The fixtures are the pinned Alpine 3.20 repository indexes, one per
//! repository, so the resolution order and the repository a candidate is
//! attributed to are exercised without a live guest.

use super::*;

use std::fs;
use std::path::PathBuf;

/// The pinned main repository index.
const MAIN_INDEX: &str = include_str!("../fixtures/APKINDEX-main");

/// The pinned community repository index.
const COMMUNITY_INDEX: &str = include_str!("../fixtures/APKINDEX-community");

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
    let path = std::env::temp_dir().join(format!("leg-guest-system-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create scratch directory");
    Scratch { path }
}

#[test]
fn the_main_index_offers_the_main_packages_against_the_main_repository() {
    let entries = index::parse_index_text(MAIN_INDEX);

    let bash = candidate_in(&entries, "bash", "x86", REPOSITORY_URLS[0]).expect("bash is offered");
    assert_eq!(bash.version, "5.2.26-r0");
    assert_eq!(bash.repository, REPOSITORY_URLS[0]);
    assert_eq!(bash.architecture, "x86");
    assert_eq!(bash.download_bytes, 459_098);
    assert_eq!(bash.install_bytes, 1_417_216);
    assert_eq!(
        bash.checksum,
        "sha1:5313307c0b7ae9288682f6402ade02cbf45042b7"
    );

    let python =
        candidate_in(&entries, "python3", "x86", REPOSITORY_URLS[0]).expect("python3 is offered");
    assert_eq!(python.version, "3.12.13-r0");
    assert_eq!(python.repository, REPOSITORY_URLS[0]);
}

#[test]
fn the_community_index_offers_the_community_packages_against_the_community_repository() {
    let entries = index::parse_index_text(COMMUNITY_INDEX);

    let ffmpeg =
        candidate_in(&entries, "ffmpeg", "x86", REPOSITORY_URLS[1]).expect("ffmpeg is offered");
    assert_eq!(ffmpeg.version, "6.1.1-r8");
    assert_eq!(ffmpeg.repository, REPOSITORY_URLS[1]);
    assert_eq!(ffmpeg.download_bytes, 245_732);
    assert_eq!(ffmpeg.install_bytes, 626_688);

    let ripgrep =
        candidate_in(&entries, "ripgrep", "x86", REPOSITORY_URLS[1]).expect("ripgrep is offered");
    assert_eq!(ripgrep.version, "14.1.0-r0");
    assert_eq!(ripgrep.repository, REPOSITORY_URLS[1]);
}

#[test]
fn a_package_is_offered_by_the_repository_that_carries_it_only() {
    let main = index::parse_index_text(MAIN_INDEX);
    let community = index::parse_index_text(COMMUNITY_INDEX);

    assert!(
        candidate_in(&main, "ffmpeg", "x86", REPOSITORY_URLS[0]).is_none(),
        "the main index offered a community package"
    );
    assert!(
        candidate_in(&community, "bash", "x86", REPOSITORY_URLS[1]).is_none(),
        "the community index offered a main package"
    );
    assert!(
        candidate_in(&main, "bash", "aarch64", REPOSITORY_URLS[0]).is_none(),
        "the main index offered a package for another architecture"
    );
}

#[test]
fn every_allowlisted_package_is_offered_by_exactly_one_pinned_repository() {
    let main = index::parse_index_text(MAIN_INDEX);
    let community = index::parse_index_text(COMMUNITY_INDEX);

    for name in crate::guest::allowed_names() {
        let offered = [
            candidate_in(&main, name, "x86", REPOSITORY_URLS[0]).is_some(),
            candidate_in(&community, name, "x86", REPOSITORY_URLS[1]).is_some(),
        ];
        assert_eq!(
            offered.iter().filter(|found| **found).count(),
            1,
            "{name} is offered by {offered:?}"
        );
    }
}

#[test]
fn the_repositories_file_holds_only_the_repository_it_was_given() {
    let scratch = scratch("repositories");

    let path = SystemGuest::write_repositories(&scratch.path, &[REPOSITORY_URLS[0]])
        .expect("the repositories file is written");
    let text = fs::read_to_string(&path).expect("the repositories file is read");
    assert_eq!(text, format!("{}\n", REPOSITORY_URLS[0]));
    assert!(
        !text.contains("community"),
        "the repositories file named another repository"
    );

    let path = SystemGuest::write_repositories(&scratch.path, &REPOSITORY_URLS)
        .expect("the repositories file is written");
    let text = fs::read_to_string(&path).expect("the repositories file is read");
    assert_eq!(
        text,
        format!("{}\n{}\n", REPOSITORY_URLS[0], REPOSITORY_URLS[1])
    );
}

#[test]
fn the_cached_index_is_found_only_when_it_is_the_only_one() {
    let scratch = scratch("index");
    let cache = scratch.path.join("cache");
    fs::create_dir_all(&cache).expect("create the cache directory");

    assert!(
        find_index(&cache).is_none(),
        "an empty cache holds an index"
    );

    let one = cache.join("APKINDEX.abc123.tar.gz");
    fs::write(&one, b"index").expect("write the index");
    assert_eq!(find_index(&cache), Some(one));

    let two = cache.join("APKINDEX.def456.tar.gz");
    fs::write(&two, b"index").expect("write the index");
    assert!(
        find_index(&cache).is_none(),
        "an ambiguous cache resolved to one index"
    );
}

#[test]
fn the_block_count_and_the_block_size_are_widened_without_casting() {
    assert_eq!(bytes_of(3u32, 512u32), 1_536);
    assert_eq!(bytes_of(3u64, 512u64), 1_536);
    assert_eq!(bytes_of(u64::MAX, 2u64), u64::MAX);
}
