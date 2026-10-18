use libdeno::{deno_core, run, ExtensionFactory, LibdenoOptions};

#[deno_core::op2]
#[string]
fn op_host_greeting() -> String {
    "hello from Rust".to_string()
}

deno_core::extension!(host_greeting,
    ops = [op_host_greeting],
    esm_entry_point = "ext:host_greeting/init.js",
    esm = [dir "examples/native_extension", "init.js"],
);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = LibdenoOptions {
        allow_all_permissions: true,
        extensions: Some(ExtensionFactory::new(|| vec![host_greeting::init()])),
        ..Default::default()
    };
    run("examples/native_extension/main.js", &options)?;
    Ok(())
}
