pub mod auth;
pub mod config;
pub mod policy;
pub mod service;

pub mod pb {
    tonic::include_proto!("rusti2.v1");
}
