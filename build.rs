use std::process::Command;

fn main() {
    let src_dir = "/home/black/Documents/bcachefs-tools/c_src";
    let out_dir = std::env::var("OUT_DIR").unwrap();

    let objs: Vec<String> = std::fs::read_dir(src_dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            if path.extension().map_or(false, |ext| ext == "o") {
                Some(path.to_str().unwrap().to_string())
            } else {
                None
            }
        })
        .collect();

    let archive = format!("{}/libbcachefs.a", out_dir);
    let mut ar = Command::new("ar");
    ar.arg("crs").arg(&archive);
    for obj in &objs {
        ar.arg(obj);
    }
    let status = ar.status().expect("failed to create archive");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=bcachefs");
    println!("cargo:rerun-if-changed={}", src_dir);
}
