use super::revoked::Revoked;
use super::*;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deck-credentials-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn names_have_the_shape_of_environment_variables() {
    for good in ["OPENAI_API_KEY", "DECK_GITHUB_TOKEN", "A", "K9"] {
        assert!(valid_name(good), "{good}");
    }
    let long = format!("A{}", "B".repeat(64));
    for bad in ["", "openai", "9KEY", "_KEY", "KEY-1", "KEY NAME", long.as_str()] {
        assert!(!valid_name(bad), "{bad}");
    }
    assert!(valid_name(&format!("A{}", "B".repeat(63))));
}

#[test]
fn a_fingerprint_is_twelve_hex_digits_of_sha256() {
    // sha256("abc") = ba7816bf8f01cfea…
    assert_eq!(fingerprint("abc"), "sha256:ba7816bf8f01");
    assert_ne!(fingerprint("key-1"), fingerprint("key-2"));
}

#[test]
fn the_file_store_sets_reads_lists_and_deletes_at_0600() {
    let dir = scratch("file");
    let path = dir.join("credentials.json");
    let store = Store::file(path.clone()).unwrap();
    assert_eq!(store.read("OPENAI_API_KEY"), Err(Absent::Missing));
    store.set("OPENAI_API_KEY", "sk-one").unwrap();
    store.set("DECK_GITHUB_TOKEN", "ghp-two").unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(store.read("OPENAI_API_KEY").unwrap(), "sk-one");
    store.set("OPENAI_API_KEY", "sk-rotated").unwrap();
    assert_eq!(store.read("OPENAI_API_KEY").unwrap(), "sk-rotated", "every read goes to the store");
    assert_eq!(store.names().unwrap(), ["DECK_GITHUB_TOKEN", "OPENAI_API_KEY"]);
    assert!(store.delete("OPENAI_API_KEY").unwrap());
    assert!(!store.delete("OPENAI_API_KEY").unwrap());
    assert_eq!(store.read("OPENAI_API_KEY"), Err(Absent::Missing));
    assert!(store.set("lowercase", "x").is_err());
    assert!(store.set("EMPTY", "").is_err());
}

#[test]
fn a_file_with_group_or_other_bits_is_refused() {
    let dir = scratch("mode");
    let path = dir.join("credentials.json");
    std::fs::write(&path, r#"{"OPENAI_API_KEY":"sk"}"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let error = Store::file(path.clone()).err().unwrap();
    assert!(format!("{error:#}").contains("chmod 600"), "{error:#}");

    // A file that gains bits after start stops serving rather than serving
    // from a readable file.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = Store::file(path.clone()).unwrap();
    assert_eq!(store.read("OPENAI_API_KEY").unwrap(), "sk");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(store.read("OPENAI_API_KEY"), Err(Absent::Unavailable(_))));

    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(Store::file(link).is_err(), "a symlink is not a regular file");
}

#[test]
fn a_malformed_file_never_echoes_its_contents() {
    let dir = scratch("malformed");
    let path = dir.join("credentials.json");
    std::fs::write(&path, "sk-secret-value").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = Store::file(path).unwrap();
    let Err(Absent::Unavailable(why)) = store.read("OPENAI_API_KEY") else { panic!("expected unavailable") };
    assert!(!why.contains("sk-secret"), "{why}");
}

#[test]
fn a_read_that_misses_the_bound_is_unavailable_not_missing() {
    let store = Store::scripted(Duration::from_millis(50), |_| {
        std::thread::sleep(Duration::from_millis(400));
        Ok("late".into())
    });
    let Err(Absent::Unavailable(why)) = store.read("OPENAI_API_KEY") else { panic!("expected unavailable") };
    assert!(why.contains("did not answer"), "{why}");
}

#[test]
fn one_read_per_name_is_outstanding_and_its_answer_serves_the_waiters() {
    let reads = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Store::scripted(Duration::from_secs(2), {
        let reads = reads.clone();
        move |_| {
            reads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            Ok("value".into())
        }
    }));
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.read("OPENAI_API_KEY"))
        })
        .collect();
    for waiter in waiters {
        assert_eq!(waiter.join().unwrap().unwrap(), "value");
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1, "concurrent requests join one read");
    // Once answered, the next request reads the store again.
    assert_eq!(store.read("OPENAI_API_KEY").unwrap(), "value");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

#[test]
fn a_revocation_is_permanent_and_survives_reopening() {
    let dir = scratch("revoked");
    let revoked = Revoked::open(&dir).unwrap();
    let first = revoked.revoke("OPENAI_API_KEY", "sha256:aaaaaaaaaaaa").unwrap();
    assert_eq!(revoked.revoke("OPENAI_API_KEY", "sha256:aaaaaaaaaaaa").unwrap(), first, "idempotent");
    revoked.revoke("DECK_GITHUB_TOKEN", "sha256:bbbbbbbbbbbb").unwrap();
    let reopened = Revoked::open(&dir).unwrap();
    assert!(reopened.is_revoked("sha256:aaaaaaaaaaaa"));
    assert!(!reopened.is_revoked("sha256:cccccccccccc"));
    assert_eq!(reopened.for_names(&["OPENAI_API_KEY".into()]), [first]);
    assert_eq!(reopened.all().len(), 2);

    std::fs::write(dir.join("credentials/revoked.json"), "not json").unwrap();
    assert!(Revoked::open(&dir).is_err(), "an unreadable list is never an empty one");
}
