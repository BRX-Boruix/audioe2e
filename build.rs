//! 用户态程序链接脚本注入（与 init/shell/synce2e 同制）。
//!
//! `-no-pie`：强制生成 ET_EXEC（非 PIE）。rust-lld 默认生成 PIE（ET_DYN），
//! 而内核 ELF 加载器只接受 ET_EXEC；裸机静态程序无需重定位，故关闭 PIE。

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rustc-link-arg=-no-pie");
}
