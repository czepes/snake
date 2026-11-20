fn main() {
    let proto_dir = "../src/main/protobuf";
    println!("cargo:rerun-if-changed={proto_dir}/snakes.proto");
    protobuf_codegen::Codegen::new()
        .pure()
        .includes([proto_dir])
        .input(format!("{proto_dir}/snakes.proto"))
        .cargo_out_dir("protos")
        .run_from_script();

    // The generated file starts with crate-level inner attributes (`#![...]`)
    // and doc comments, which are only legal at the top of a file compiled as
    // its own module (`#[path = ...] mod proto;`), not via `include!`. Strip
    // them so the file can be included verbatim from `src/main.rs`.
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let generated = std::path::Path::new(&out_dir).join("protos/snakes.rs");
    let contents = std::fs::read_to_string(&generated).expect("read generated protobuf code");
    let stripped: String = contents
        .lines()
        .filter(|line| !line.starts_with("#![") && !line.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&generated, stripped).expect("write stripped protobuf code");
}
