//! GUI compatibility re-export.
//!
//! The implementation lives in `vida-client` so the GUI, `vidactl`, and MCP
//! bridge cannot drift onto different daemon authentication/protocol paths.

pub use vida_client::{AgentApproval, AgentHostDraft, OwnerEvent, PushMsg, WsClient};
