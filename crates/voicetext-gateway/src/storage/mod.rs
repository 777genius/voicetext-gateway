//! Durable outbound storage adapters for authoritative batch jobs.

mod postgres;
mod records;
mod spool;
mod spool_maintenance;

pub use postgres::PostgresBatchJobStore;
pub use spool::DurableFileSpool;
pub use spool_maintenance::SpoolMaintenanceReport;
