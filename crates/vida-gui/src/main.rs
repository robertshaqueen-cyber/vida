mod app;
mod screens;
mod secure_text_input;
mod toast;
mod ws_client;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("vida GUI error: {:?}", e);
        std::process::exit(1);
    }
}
