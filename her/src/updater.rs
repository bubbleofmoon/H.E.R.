#![cfg_attr(windows, windows_subsystem = "windows")]
mod handover;
mod install_root;
mod process_identity;
mod release_store;
mod release_signature;
mod subprocess;
mod update;

fn main() {
    subprocess::prevent_stdio_inheritance();
    let mut args: Vec<String> = std::env::args().collect();
    let result = install_root::InstallRoot::select(&mut args).and_then(|root| {
        std::env::set_var("HER_INSTALL_ROOT", subprocess::external_path(root.root())?);
        Ok(handover::run_helper(&args[1..]))
    });
    std::process::exit(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("[herupdate] {error}");
            2
        }
    });
}
