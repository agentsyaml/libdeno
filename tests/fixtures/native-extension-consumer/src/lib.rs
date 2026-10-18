use libdeno::{ExtensionFactory, LibdenoOptions};

#[deno_core::op2]
#[string]
fn op_host_greeting() -> String {
    "hello from Rust".to_string()
}

deno_core::extension!(host_greeting, ops = [op_host_greeting]);

pub fn options_with_host_extension() -> LibdenoOptions {
    LibdenoOptions {
        extensions: Some(ExtensionFactory::new(|| vec![host_greeting::init()])),
        ..Default::default()
    }
}
