/// Cargo silently ignores a `[patch]` whose version no longer matches, which
/// would quietly drop the font memory fix, so fail the build instead.
fn check_vendored_slint() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=vendor/i-slint-renderer-skia/Cargo.toml");
    let version_of = |text: &str, name: &str| {
        let start = text.find(&format!("name = \"{name}\"\nversion = \""))?;
        let rest = &text[start..].split_once("version = \"")?.1;
        Some(rest[..rest.find('"')?].to_owned())
    };
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .replace("\r\n", "\n")
    };
    let slint = version_of(&read("Cargo.lock"), "i-slint-core");
    let vendored = version_of(
        &read("vendor/i-slint-renderer-skia/Cargo.toml"),
        "i-slint-renderer-skia",
    );
    if slint != vendored {
        panic!(
            "Slint is {} but vendor/i-slint-renderer-skia is {}; \
             run scripts/update-slint-patch.ps1",
            slint.as_deref().unwrap_or("unknown"),
            vendored.as_deref().unwrap_or("missing"),
        );
    }
}

fn main() {
    check_vendored_slint();

    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("icons/icon.ico")
        .set("FileDescription", "YouTube Music")
        .set("ProductName", "YouTube Music");
    res.compile().expect("Windows resources compile");
}
