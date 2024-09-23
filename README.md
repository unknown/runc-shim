# runc-shim

Shim process to interact with runc.

## Building

```bash
cargo build --release
```

## Running

```bash
sudo ./target/release/shim --bundle /tmp/bundle --id container-id
```
