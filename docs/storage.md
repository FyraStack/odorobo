# Storage Integration

Orodobo adds a storage integration layer on top of Cloud Hypervisor's block device support, using config transformers to convert custom URI schemes into valid block device paths for Cloud Hypervisor.
This allows users to specify storage devices in a more flexible way, and enables simpler integration with networked storage backends, by having Odorobo abstract and attach/detach storage devices on demand based on the VM's lifecycle and provisioning hooks.

Currently, Odorobo's Storage transformer supports the following backends:

- **Local Disk Images**: Specify a local disk image with the `file://` URI scheme, e.g. `file:///var/lib/odorobo/images/my-disk.qcow2`. This is simply passed through to Cloud Hypervisor as if the `file://` prefix was not there, implemented as a dummy transformer for validating the storage transformer API.
- **Ceph RBD**: Specify a Ceph RBD image with the `rbd://` URI scheme, e.g. `rbd://my-pool/my-image`. Odorobo will use the `rbd` command line tool to map the RBD image to a local block device (e.g. `/dev/rbd0`), and then pass this block device path to Cloud Hypervisor.
- **iSCSI**: Specify an iSCSI target with the `iscsi://` URI scheme, e.g. `iscsi://<target-ip>:<port>/<iqn>/<lun>`. Odorobo will use the `iscsiadm` command line tool to log in to the iSCSI target and map the specified LUN to a local block device (e.g. `/dev/disk/by-path/ip-<target-ip>:<port>-iscsi-<iqn>-lun-<lun>`), and then pass this block device path to Cloud Hypervisor.

To use the storage transformers, simply specify the desired URI in place of the `path` field for a block device in your VM config. For example:

```json
{
  ...
  "disks": [
    {
      "path": "rdb://my-pool/my-image",
      "id": "disk0",
    }
  ],
}
```

Odorobo will automatically detect the `rbd://` scheme, map the RBD image to a local block device, and pass the block device path to Cloud Hypervisor when creating the VM, transforming them to:

```json
{
  ...
  "disks": [
    {
      "path": "/dev/rbd/my-pool/my-image",
      "id": "rdb://my-pool/my-image?id=disk0",
    }
  ],
}
```

The `id` field is transformed to include the original URI for reference, and can be used in provisioning hooks to identify which disk is which when attaching/detaching storage devices on demand.


> [!NOTE]
> Ceph RBD integration requires the following udev rule to allow Odorobo to easily find the mapped block device for a given RBD image:
>
> ```
> # /etc/udev/rules.d/50-rbd.rules
> KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="disk", PROGRAM="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c"
> KERNEL=="rbd[0-9]*", ENV{DEVTYPE}=="partition", PROGRAM="/usr/bin/ceph-rbdnamer %k", SYMLINK+="rbd/%c-part%n"
> ```
> 
> This rule is commonly included in Ceph packages (`ceph-common` in Fedora), but if your distribution does not include it by default, you will need to add it manually for RBD support to work properly. This allows Odorobo to find the mapped block device for a given RBD image under `/dev/rbd/`, which is necessary for passing the correct block device path to Cloud Hypervisor.
