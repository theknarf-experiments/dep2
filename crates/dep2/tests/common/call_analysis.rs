//! Shared real-engine harness for scoped call analysis tests.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dep2_core::engine::{
    decode_state_row, live_rows, Dep2, Dep2Config, RelationState, RelationTypes,
};
use dep2_plugin_treesitter::TreeSitterPlugin;

type Rows = Vec<Vec<String>>;
pub struct Running {
    state: Arc<Mutex<RelationState>>,
    types: Arc<RelationTypes>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Result<(), String>>>,
}
impl Running {
    pub fn start(root: &Path, languages: &[(&str, &str)]) -> Self {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut engine = Dep2::with_config(Dep2Config {
            workers: 1,
            print_updates: false,
            publish: false,
        });
        engine.add_plugin(Box::new(TreeSitterPlugin));
        let grammars = languages
            .iter()
            .map(|(ext, lang)| {
                let p = repo.join(format!("grammars/tree-sitter-{lang}.wasm"));
                assert!(p.is_file(), "missing {}; run mise run setup", p.display());
                format!("{ext}={}", p.display())
            })
            .collect::<Vec<_>>()
            .join(",");
        engine.add_source(
            None,
            "treesitter",
            HashMap::from([
                (
                    "root".into(),
                    root.canonicalize().unwrap().display().to_string(),
                ),
                ("grammars".into(), grammars),
            ]),
        );
        engine
            .load_program_file(&repo.join("examples/calls.dl"))
            .unwrap();
        let state = engine.state();
        let types = engine.relation_types();
        let stop = Arc::new(AtomicBool::new(false));
        let sd = stop.clone();
        let handle = Some(thread::spawn(move || engine.run(sd)));
        Self {
            state,
            types,
            stop,
            handle,
        }
    }
    pub fn rows(&self, relation: &str) -> Rows {
        let state = self.state.lock().unwrap();
        state
            .get(relation)
            .into_iter()
            .flat_map(|r| live_rows(r))
            .map(|r| decode_state_row(r, &self.types[relation]))
            .collect()
    }
    pub fn wait_for(&self, mut check: impl FnMut() -> Result<(), String>) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut consecutive = 0;
        let mut attempts = 0;
        loop {
            let result = check();
            attempts += 1;
            if attempts % 100 == 0 {
                eprintln!("waiting for graph: {result:?}");
            }
            if result.is_ok() {
                consecutive += 1;
                if consecutive == 4 {
                    return;
                }
            } else {
                consecutive = 0;
            }
            assert!(
                !self.handle.as_ref().unwrap().is_finished(),
                "engine stopped"
            );
            assert!(
                Instant::now() < deadline,
                "{}",
                result.err().unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let result = h.join();
            if !thread::panicking() {
                result.unwrap().unwrap();
            }
        }
    }
}
