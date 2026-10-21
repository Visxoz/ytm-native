fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("icons/icon.ico")
        .set("FileDescription", "YouTube Music")
        .set("ProductName", "YouTube Music");
    res.compile().expect("Windows resources compile");
}
