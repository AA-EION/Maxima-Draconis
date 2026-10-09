#![feature(type_ascription)]
#![feature(slice_pattern)]
#![feature(string_remove_matches)]
#![feature(trait_alias)]
#![feature(type_alias_impl_trait)]

pub mod auth_server;
pub mod content;
pub mod core;
pub mod gameinfo;
pub mod lsx;
pub mod ooa;
pub mod presence;
pub mod rtm;
pub mod server_client;
pub mod steam;
pub mod util;

#[cfg(unix)]
pub mod unix;

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("Only x86_64 and aarch64 are supported at the moment");
