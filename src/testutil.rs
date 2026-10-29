//! Shared test setup: one isolated JOCKEY_HOME for the whole test process.

use std::sync::OnceLock;

pub fn env() -> &'static tempfile::TempDir {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any test thread reads these variables.
        unsafe {
            std::env::set_var("JOCKEY_HOME", d.path());
            std::env::set_var("JOCKEY_MOCK_DELAY_MS", "3");
        }
        d
    })
}
