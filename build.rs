//! Генерация protobuf-типов. `src/snakes.v2.rs` — результат работы prost,
//! а ручные доменные типы и конверсии находятся в `src/wire.rs`.

const PROTOBUF: &str = "./protobuf";
const SNAKES_PROTO: &str = "./protobuf/snakes.proto";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let include = PROTOBUF;
    let proto = SNAKES_PROTO;
    println!("cargo:rerun-if-changed={proto}");
    prost_build::Config::new()
        .out_dir("src")
        .compile_protos(&[&proto], &[&include])?;
    Ok(())
}
