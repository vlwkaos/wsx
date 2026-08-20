use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/store-contract-tests")
            .join(format!(
                "{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn given_existing_destination_when_atomically_created_then_original_is_not_replaced() {
    let fixture = TestDir::new("existing");
    let destination = fixture.root.join("state.toml");
    fs::write(&destination, b"original").unwrap();

    let result = atomic_create(&destination, b"replacement");

    assert_eq!(
        (result.unwrap_err().kind(), fs::read(destination).unwrap()),
        (std::io::ErrorKind::AlreadyExists, b"original".to_vec())
    );
}
