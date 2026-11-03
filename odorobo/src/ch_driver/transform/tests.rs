use super::{ConfigTransform, TransformChain};
use cloud_hypervisor_client::models::VmConfig;
use std::sync::{Arc, Mutex};

struct CleanupProbe {
    id: u8,
    fail: bool,
    calls: Arc<Mutex<Vec<u8>>>,
}
impl ConfigTransform for CleanupProbe {
    fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        Ok(())
    }
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        self.calls
            .lock()
            .map_err(|error| stable_eyre::eyre::eyre!(error.to_string()))?
            .push(self.id);
        if self.fail {
            stable_eyre::eyre::bail!("cleanup failed");
        }
        Ok(())
    }
}

#[test]
fn teardown_attempts_remaining_transforms_after_failure_in_reverse_order() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = TransformChain::new()
        .add(CleanupProbe {
            id: 1,
            fail: false,
            calls: Arc::clone(&calls),
        })
        .add(CleanupProbe {
            id: 2,
            fail: true,
            calls: Arc::clone(&calls),
        });
    assert!(chain.teardown("test", &mut VmConfig::default()).is_err());
    assert_eq!(*calls.lock().expect("inspect calls"), vec![2, 1]);
}
