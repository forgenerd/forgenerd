//! Ensures `web/dist/index.html` exists so the crate builds without Node installed.
//! The real UI is produced by `pnpm --dir web build`.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=migrations");
    let index = Path::new("web/dist/index.html");
    if !index.exists() {
        let placeholder = "<!doctype html><html><head><meta charset=\"utf-8\"><title>forgenerd</title></head>\
            <body><p>forgenerd UI not built. Run <code>pnpm --dir web build</code> and rebuild.</p></body></html>";
        if let Err(err) =
            std::fs::create_dir_all("web/dist").and_then(|()| std::fs::write(index, placeholder))
        {
            println!("cargo:warning=could not write placeholder UI: {err}");
        }
    }
}
