mod common;

use std::{
    io::{self, Write},
    sync::{Arc, Mutex, OnceLock},
};

use cellar::{db::NewProject, projects::audit_orphan_project_directories};
use common::TestContext;
use uuid::Uuid;

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static CAPTURED_LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    CAPTURED_LOGS
        .get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = CapturedWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            logs
        })
        .clone()
}

#[tokio::test]
async fn startup_project_audit_reports_only_orphans_and_changes_nothing() {
    let logs = captured_logs();
    logs.lock().unwrap().clear();
    let context = TestContext::new().await;
    let committed = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000020").unwrap();
    let orphan = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000021").unwrap();
    context.storage.create_project_dir(committed).await.unwrap();
    context.storage.create_project_dir(orphan).await.unwrap();
    context
        .database
        .create_project(NewProject::new(committed, "private project name").unwrap())
        .await
        .unwrap();

    audit_orphan_project_directories(&context.database, context.storage.as_ref())
        .await
        .unwrap();

    assert_eq!(
        context
            .database
            .list_projects()
            .await
            .unwrap()
            .iter()
            .map(|project| project.id())
            .collect::<Vec<_>>(),
        [committed]
    );
    assert!(context.project_path(committed).is_dir());
    assert!(context.project_path(orphan).is_dir());
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let matching = logs
        .lines()
        .filter(|line| line.contains(&orphan.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "unexpected audit logs: {logs}");
    assert!(matching[0].contains("reason=\"orphan_project_directory\""));
    assert!(matching[0].contains("manual cleanup"));
    assert!(!logs.contains(&committed.to_string()));
    assert!(!logs.contains("private project name"));
    assert!(!logs.contains(context.temp.path().to_string_lossy().as_ref()));
    context.close().await;
}
