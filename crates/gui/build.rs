fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_default_translation_context(slint_build::DefaultTranslationContext::None)
        .with_bundled_translations("lang");
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
    println!("cargo:rerun-if-changed=lang");
}
