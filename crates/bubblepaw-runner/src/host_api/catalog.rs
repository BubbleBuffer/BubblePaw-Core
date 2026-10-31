//! `tools.list` and `skills.list`: what the Desktop tool and skill settings pages show.
use super::*;
use crate::{live_config::SkillList, toolsets::ToolInfo};

/// Every published tool, whatever the main chat's toolset.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolsResult {
    pub tools: Vec<ToolInfo>,
}
pub type SkillsResult = SkillList;

fn live(ctx: &HostCtx) -> Result<&crate::live_config::LiveConfig, HostError> {
    required(&ctx.live_config, "Live config")
}
pub fn tools(ctx: &HostCtx, _input: EmptyParams) -> Result<ToolsResult, HostError> {
    Ok(ToolsResult {
        tools: live(ctx)?.tool_list(),
    })
}
pub fn skills(ctx: &HostCtx, _input: EmptyParams) -> Result<SkillsResult, HostError> {
    live(ctx)?
        .skill_list()
        .map_err(|error| HostError::internal(format!("{error:#}")))
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    match method {
        MethodId::ToolsList => encode(tools(ctx, decode(params)?)?),
        _ => encode(skills(ctx, decode(params)?)?),
    }
}
