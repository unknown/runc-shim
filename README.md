# runc-shim

Shim process to interact with an OCI runtime. The design is loosely based on [containerd's shim](https://github.com/containerd/containerd/blob/main/core/runtime/v2/README.md).

## Building

```bash
cargo build --release
```

## Running

```bash
sudo ./target/release/shim --runtime /usr/sbin/runc
```
