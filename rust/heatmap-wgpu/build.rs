fn main() {
    #[cfg(windows)]
    {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("l4j")
            .join("Juicebox.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(icon.to_str().expect("icon path is not UTF-8"));
        resource.set("ProductName", "Juicebox Rust");
        resource.set("FileDescription", "Juicebox Rust Hi-C Viewer");
        resource.set("LegalCopyright", "Juicebox contributors");
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
