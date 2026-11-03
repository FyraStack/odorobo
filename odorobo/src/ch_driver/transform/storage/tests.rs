use super::{StorageDriver, StorageDriverTransformer};
use async_trait::async_trait;
use cloud_hypervisor_client::models::{DiskConfig, VmConfig};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use url::Url;

struct FailOnceStorage {
    calls: Arc<Mutex<Vec<String>>>,
    released: Mutex<HashSet<String>>,
    failed_once: Mutex<bool>,
}
#[async_trait]
impl StorageDriver for FailOnceStorage {
    fn scheme(&self) -> &'static str {
        "test"
    }
    async fn resolve(&self, _uri: &Url) -> stable_eyre::Result<PathBuf> {
        Ok(PathBuf::new())
    }
    async fn release(&self, uri: &Url) -> stable_eyre::Result<()> {
        let key = uri.as_str().to_owned();
        self.calls
            .lock()
            .map_err(|e| stable_eyre::eyre::eyre!(e.to_string()))?
            .push(key.clone());
        if uri.path() == "/a" {
            let mut failed = self
                .failed_once
                .lock()
                .map_err(|e| stable_eyre::eyre::eyre!(e.to_string()))?;
            if !*failed {
                *failed = true;
                drop(failed);
                stable_eyre::eyre::bail!("transient release error");
            }
        }
        if !self
            .released
            .lock()
            .map_err(|e| stable_eyre::eyre::eyre!(e.to_string()))?
            .insert(key)
        {
            stable_eyre::eyre::bail!("duplicate release");
        }
        Ok(())
    }
}

#[tokio::test]
async fn partial_release_can_be_retried_without_releasing_completed_disks_again() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let transformer = StorageDriverTransformer::new().with_backend(FailOnceStorage {
        calls: Arc::clone(&calls),
        released: Mutex::new(HashSet::new()),
        failed_once: Mutex::new(false),
    });
    let mut config = VmConfig {
        disks: Some(
            ["a", "b"]
                .iter()
                .map(|name| DiskConfig {
                    path: Some(format!("/dev/{name}")),
                    id: Some(format!("test://disk/{name}")),
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    };
    assert!(transformer.release_config(&mut config).await.is_err());
    transformer
        .release_config(&mut config)
        .await
        .expect("remaining release succeeds on retry");
    assert_eq!(
        *calls.lock().expect("inspect release calls"),
        vec!["test://disk/a", "test://disk/b", "test://disk/a"]
    );
    assert!(
        config
            .disks
            .expect("disks")
            .iter()
            .all(|disk| disk.path.is_none() && disk.id.is_none())
    );
}
