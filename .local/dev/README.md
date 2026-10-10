# Local development stack

Linux development stack using rootful Podman Compose (Docker Compose v2 also works). Ceph and Odorobo use privileged containers; load the host `loop` and `rbd` modules first.

```bash
sudo modprobe loop rbd
sudo bash .local/dev/init.sh
```

The stack starts a single-node Ceph cluster, Odorobo, and persistent local etcd. The agent defaults to `http://etcd:2379` and the stable node ID `odorobo`. For an external store, pass `ODOROBO_ETCD_ENDPOINTS` and a stable, unique `ODOROBO_HOSTNAME` (the local etcd service still starts):

```bash
sudo env ODOROBO_ETCD_ENDPOINTS=http://etcd.example.net:2379 ODOROBO_HOSTNAME=node-1 bash .local/dev/init.sh
```

Every node in one cluster must use the same etcd store and its own stable node ID.

The etcd data is kept in the `etcd_data` named volume. Stop and restart without deleting state:

```bash
sudo podman compose -f .local/dev/compose.yml down
sudo bash .local/dev/init.sh
```

To inspect logs or reset the local environment:

```bash
sudo podman compose -f .local/dev/compose.yml logs -f etcd ceph odorobo
sudo podman compose -f .local/dev/compose.yml down --remove-orphans --volumes
```

`down --volumes` is destructive: it deletes both etcd cluster state and Ceph data. The stack is for development, not production; it does not provide multi-manager fencing or automatic VM recovery. `test.sh` is an optional manual Ceph/RBD guest boot test.
