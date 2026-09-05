fn main() {
    println!("cargo:rerun-if-changed=assets/shiyue-icon.ico");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/shiyue-icon.ico");
        resource.set("FileDescription", "拾阅 Shiyue RSS Reader");
        resource.set("ProductName", "拾阅 Shiyue");
        resource.set("CompanyName", "Shiyue");
        resource.set("LegalCopyright", "Copyright (c) Shiyue contributors");
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
