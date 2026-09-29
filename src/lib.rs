pub mod auth;
pub mod config;
pub mod policy;
pub mod service;
pub mod telemetry;

pub mod pb {
    pub use cotab_proto::rusti2::v1::*;
}
