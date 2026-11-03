use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use cloud_hypervisor_client::models::{VmConfig, VmInfo, VmState};
use http_body_util::Full;
use hyper::{Response, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::UnixListener;

use super::VMInstance;
use crate::ch_driver::{
    provisioning::hooks::{HookManager, ProvisioningHook},
    transform::{ConfigTransform, TransformChain},
};

struct FailingHook;
#[async_trait]
impl ProvisioningHook for FailingHook {
    async fn before_stop(&self, _vmid: &str, _config: &VmInfo) -> stable_eyre::Result<()> {
        stable_eyre::eyre::bail!("before-stop failure");
    }
    async fn after_stop(&self, _vmid: &str, _config: &VmConfig) -> stable_eyre::Result<()> {
        stable_eyre::eyre::bail!("after-stop failure");
    }
}

struct ReleaseProbe {
    released: Arc<AtomicBool>,
    fail: bool,
}
impl ConfigTransform for ReleaseProbe {
    fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        Ok(())
    }
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        self.released.store(true, Ordering::SeqCst);
        if self.fail {
            stable_eyre::eyre::bail!("release failure");
        }
        Ok(())
    }
}

struct ProcessOrderProbe {
    child: Arc<tokio::sync::Mutex<tokio::process::Child>>,
    released: Arc<AtomicBool>,
}
impl ConfigTransform for ProcessOrderProbe {
    fn transform(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        Ok(())
    }
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> stable_eyre::Result<()> {
        let mut child = self
            .child
            .try_lock()
            .map_err(|e| stable_eyre::eyre::eyre!(e.to_string()))?;
        if child.try_wait()?.is_none() {
            stable_eyre::eyre::bail!("resource release attempted before VMM exit");
        }
        drop(child);
        self.released.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn mock_vmm(
    socket: &std::path::Path,
    stopped: Arc<AtomicBool>,
    fail_shutdown: bool,
) -> tokio::task::JoinHandle<()> {
    mock_vmm_with_state(socket, stopped, fail_shutdown, VmState::Created)
}

fn mock_vmm_with_state(
    socket: &std::path::Path,
    stopped: Arc<AtomicBool>,
    fail_shutdown: bool,
    state: VmState,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind mock VMM");
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("mock VMM connection");
            let stopped = Arc::clone(&stopped);
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                    let stopped = Arc::clone(&stopped);
                    async move {
                        let (status, body) = match request.uri().path() {
                            "/api/v1/vm.info" => (
                                200,
                                serde_json::to_string(&VmInfo::new(VmConfig::default(), state))
                                    .expect("serialize VM info"),
                            ),
                            "/api/v1/vmm.shutdown" if !fail_shutdown => {
                                stopped.store(true, Ordering::SeqCst);
                                (204, String::new())
                            }
                            "/api/v1/vm.shutdown" => {
                                std::future::pending::<()>().await;
                                unreachable!("shutdown deliberately never responds");
                            }
                            _ => (500, "API failure".to_owned()),
                        };
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(body)))
                                .expect("mock response"),
                        )
                    }
                });
                _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    })
}

fn fixture(fail_release: bool) -> (VMInstance, Arc<AtomicBool>) {
    let dir = std::env::temp_dir().join(format!("odorobo-cleanup-{}", ulid::Ulid::generate()));
    std::fs::create_dir_all(&dir).expect("create test runtime");
    let released = Arc::new(AtomicBool::new(false));
    let transforms = TransformChain::new().add(ReleaseProbe {
        released: Arc::clone(&released),
        fail: fail_release,
    });
    let mut vm = VMInstance::new("test-vm", dir.join("ch.sock"), Some(transforms), None);
    vm.vm_config = Some(VmConfig::default());
    vm.hook_manager = HookManager::default().add_hook(FailingHook);
    (vm, released)
}

#[tokio::test]
async fn hook_failures_do_not_skip_vmm_shutdown_or_resource_cleanup() {
    let (mut vm, released) = fixture(false);
    let stopped = Arc::new(AtomicBool::new(false));
    let mock = mock_vmm(vm.ch_socket_path(), Arc::clone(&stopped), false);
    assert!(vm.destroy().await.is_err()); // Report the hook failure after essential cleanup.
    assert!(stopped.load(Ordering::SeqCst));
    assert!(released.load(Ordering::SeqCst));
    assert!(!vm.runtime_dir().exists());
    assert!(vm.vm_config.is_none());
    mock.abort();
}

#[tokio::test]
async fn unconfirmed_vmm_shutdown_retains_resources_and_runtime() {
    let (mut vm, released) = fixture(false);
    let mock = mock_vmm(vm.ch_socket_path(), Arc::new(AtomicBool::new(false)), true);
    assert!(vm.destroy().await.is_err());
    assert!(!released.load(Ordering::SeqCst));
    assert!(vm.runtime_dir().exists());
    assert!(vm.vm_config.is_some());
    mock.abort();
    std::fs::remove_dir_all(vm.runtime_dir()).expect("remove test runtime");
}

#[tokio::test]
async fn guest_shutdown_timeout_leaves_instance_available_for_info_and_delete() {
    let (mut vm, released) = fixture(false);
    let stopped = Arc::new(AtomicBool::new(false));
    let mock = mock_vmm(vm.ch_socket_path(), Arc::clone(&stopped), false);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(7), vm.shutdown())
            .await
            .expect("shutdown deadline should fire")
            .is_err()
    );
    vm.info()
        .await
        .expect("info remains available after shutdown timeout");
    assert!(vm.vm_config.is_some());
    assert!(!released.load(Ordering::SeqCst));
    assert!(vm.destroy().await.is_err()); // Failing hook is still reported.
    assert!(stopped.load(Ordering::SeqCst));
    assert!(released.load(Ordering::SeqCst));
    mock.abort();
}

#[tokio::test]
async fn stalled_running_guest_does_not_prevent_final_vmm_cleanup() {
    let (mut vm, released) = fixture(false);
    let stopped = Arc::new(AtomicBool::new(false));
    let mock = mock_vmm_with_state(
        vm.ch_socket_path(),
        Arc::clone(&stopped),
        false,
        VmState::Running,
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(7), vm.destroy())
            .await
            .expect("delete proceeds after guest shutdown deadline")
            .is_err()
    );
    assert!(stopped.load(Ordering::SeqCst));
    assert!(released.load(Ordering::SeqCst));
    assert!(!vm.runtime_dir().exists());
    mock.abort();
}

#[tokio::test]
async fn resource_cleanup_retry_does_not_require_a_running_vmm() {
    let (mut vm, released) = fixture(true);
    let mock = mock_vmm(vm.ch_socket_path(), Arc::new(AtomicBool::new(false)), false);
    assert!(vm.destroy().await.is_err());
    assert!(vm.vmm_stopped);
    assert!(vm.vm_config.is_some());
    mock.abort();
    mock.await.expect_err("mock was aborted");
    std::fs::remove_file(vm.ch_socket_path()).expect("remove dead VMM endpoint");
    released.store(false, Ordering::SeqCst);
    vm.transformer = TransformChain::new().add(ReleaseProbe {
        released: Arc::clone(&released),
        fail: false,
    });
    assert!(vm.destroy().await.is_err()); // after-stop hook still fails, cleanup succeeds.
    assert!(released.load(Ordering::SeqCst));
    assert!(!vm.runtime_dir().exists());
    assert!(vm.vm_config.is_none());
}

#[tokio::test]
async fn failed_resource_release_retains_cleanup_metadata_for_retry() {
    let (mut vm, released) = fixture(true);
    assert!(vm.purge_instance_data().is_err());
    assert!(released.load(Ordering::SeqCst));
    assert!(vm.vm_config.is_some());
    assert!(vm.runtime_dir().exists());
    std::fs::remove_dir_all(vm.runtime_dir()).expect("remove test runtime");
}

#[tokio::test]
async fn failed_vmm_api_still_kills_and_reaps_owned_process_before_cleanup() {
    let (mut vm, released) = fixture(false);
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn test child");
    vm.child_process = Some(Arc::new(tokio::sync::Mutex::new(child)));
    let observed_child = vm.child_process().expect("observe child process");
    vm.transformer = TransformChain::new().add(ProcessOrderProbe {
        child: Arc::clone(&observed_child),
        released: Arc::clone(&released),
    });
    let mock = mock_vmm(vm.ch_socket_path(), Arc::new(AtomicBool::new(false)), true);
    assert!(vm.destroy().await.is_err()); // Hook failure is still reported.
    assert!(
        observed_child
            .lock()
            .await
            .try_wait()
            .expect("read process status")
            .is_some()
    );
    assert!(released.load(Ordering::SeqCst));
    assert!(!vm.runtime_dir().exists());
    mock.abort();
}
