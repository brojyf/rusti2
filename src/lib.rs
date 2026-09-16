pub mod auth;
pub mod config;
pub mod policy;
pub mod service;
pub mod telemetry;

pub mod pb {
    pub use cogate_cotab_proto::rusti2::v1::*;
}
