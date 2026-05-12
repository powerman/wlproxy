use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(need_unix_ancillary_feature)");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let rustc = std::env::var("RUSTC").unwrap_or("rustc".to_string());
    let mut target = std::env::var("TARGET").unwrap();
    // TARGET may include file path suffix on some systems; strip it.
    if let Some((t, _)) = target.split_once(".json") {
        target = t.to_string();
    }

    // On older/beta nightlies this API requires `#![feature(unix_socket_ancillary_data)]`,
    // while on newer nightlies the feature gate has been removed.
    // Probe the compiler to see if the API is available without the feature gate.
    let probe_path = Path::new(&out_dir).join("probe.rs");
    let out_path = Path::new(&out_dir).join("probe.out");
    std::fs::write(
        &probe_path,
        b"fn main() {
    let mut buf = [0u8; 64];
    let mut anc = std::os::unix::net::SocketAncillary::new(&mut buf);
    let _ = anc.messages();
    let _ = anc.truncated();
    let _ = anc.clear();
    anc.add_fds(&[]);
    let _ = std::os::unix::net::AncillaryData::ScmRights([0; 0]);
}
",
    )
    .unwrap();

    let output = std::process::Command::new(&rustc)
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--target",
            &target,
            "-o",
            &out_path.to_string_lossy(),
            &probe_path.to_string_lossy(),
        ])
        .output()
        .unwrap();

    if !output.status.success() {
        println!("cargo:rustc-cfg=need_unix_ancillary_feature");
    }
}
