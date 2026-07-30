//! `antibisingd`'s IPC surface (U10), exposed as a library so integration
//! tests (`daemon/tests/ipc.rs`) can drive the real Unix-socket server
//! against scratch paths without going through `main`'s production
//! `InstallPaths`/`IpcPaths::production()` or touching the real
//! `filter-chain.service`.

pub mod ipc;
