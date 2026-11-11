//! Integration tests of one area (remote), one binary: each former test file is a module here.

#[allow(dead_code)]
#[path = "remote_tunnel/fake.rs"]
mod fake;
#[allow(dead_code)]
#[path = "remote_tunnel/fake_api.rs"]
mod fake_api;

mod remote_cli;
mod remote_cloudflare_status;
mod remote_egress;
mod remote_install;
mod remote_settings;
mod remote_tunnel;
