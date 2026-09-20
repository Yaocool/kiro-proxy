//! GitHub Copilot provider adapter.

mod account;
mod auth;
mod client;

pub use account::{CopilotAccount, CopilotAccountSummary, CopilotCredentials};
pub use auth::{DeviceLoginState, DeviceLoginStatus};
pub use client::{CopilotProvider, CopilotSettings};
