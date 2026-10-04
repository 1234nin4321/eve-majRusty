//! Platform-independent pieces of EVE-Maj Preview: shared enums, state transitions, color math
//! and logging. Nothing here touches Win32, so it builds and tests on any host.

pub mod log;
pub mod types;
pub mod state;
pub mod color;
pub mod profile_name;
pub mod virtual_keys;
pub mod display_grid;
pub mod config;
pub mod http_client;
pub mod activity_tracker;
pub mod protocol;
pub mod accounts_store;
pub mod eve_accounts;
pub mod esi_prices;
pub mod update;
