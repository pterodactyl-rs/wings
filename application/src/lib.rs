pub mod commands;
pub mod config;
pub mod deserialize;
pub mod extensions;
pub mod io;
pub mod models;
pub mod remote;
pub mod routes;
pub mod server;
pub mod ssh;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_COMMIT: &str = env!("CARGO_GIT_COMMIT");

#[inline]
pub fn is_valid_utf8_slice(s: &[u8]) -> bool {
    let mut idx = s.len();
    while idx > 0 {
        if str::from_utf8(&s[..idx]).is_ok() {
            return true;
        }

        idx -= 1;
    }

    false
}
