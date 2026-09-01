//! Durable outbound storage adapters for authoritative batch jobs.

mod postgres;
mod records;
mod spool;

pub use postgres::PostgresBatchJobStore;
pub use spool::DurableFileSpool;
