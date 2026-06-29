#![allow(clippy::question_mark)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::format_in_format_args)]

mod accounts;
mod auth;
mod cli;
mod config;
mod http;
mod json;
mod landing;
mod oauth;
mod upstream;
mod util;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("akurai-router: {err}");
        std::process::exit(1);
    }
}
