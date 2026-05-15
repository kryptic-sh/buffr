fn main() {
    cxx_build::bridge("src/ffi.rs")
        .file("src/shim/ladybird_shim.cpp")
        .std("c++17")
        .compile("buffr_ladybird_shim");

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/shim/ladybird_shim.h");
    println!("cargo:rerun-if-changed=src/shim/ladybird_shim.cpp");
}
