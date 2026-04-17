# Provisioning

## Cloud Hypervisor

To provision cloud-init-compatible images with Cloud Hypervisor, set the serial number in the VmConfig to a
cloud-init [line config](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html#line-configuration-in-detail).


```json
{
  ...
  "platform": {
    "serial_number": "ds=nocloud,..."
  },
}
```

You may set anything in there, or have a metadata service that seeds this configuration.

> [!NOTE]
> As of April 2026, this seems to only work on EDK2 firmware, rust-hypervisor-firmware does not seem to export DMI data
