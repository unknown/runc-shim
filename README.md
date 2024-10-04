# runc-shim

A shim to interact with an OCI runtime that is loosely based on the containerd shim. Read about containerd shim's design and the purpose of a shim [here](https://github.com/containerd/containerd/blob/main/core/runtime/v2/README.md).

## Usage

Build the shim by running:

```bash
cargo build --release
```

Running the shim will print to stdout a socket path that can be used to interact with the shim process via gRPC. For example to start a task with ID `hello`:

```bash
sudo ./target/release/shim --runtime /usr/sbin/runc --id hello start
# unix:///run/shim/16156531084128653017.sock
```
