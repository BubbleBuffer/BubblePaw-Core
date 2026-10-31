//! The approval policy a config section describes.
use super::*;

pub fn approval_policy(settings: &Settings) -> Result<ApprovalPolicy> {
    approval_policy_for(&settings.approvals)
}
pub(crate) fn approval_policy_for(approvals: &ApprovalSettings) -> Result<ApprovalPolicy> {
    let mode = |value: &str| match value {
        "ask" => Ok(ApprovalMode::Ask),
        "auto" => Ok(ApprovalMode::Auto),
        "allow" => Ok(ApprovalMode::Allow),
        "deny" => Ok(ApprovalMode::Deny),
        _ => Err(anyhow!("Invalid approval mode")),
    };
    Ok(ApprovalPolicy {
        confirm: mode(&approvals.confirm)?,
        tools: approvals
            .tools
            .iter()
            .map(|(name, value)| Ok((name.clone(), mode(value)?)))
            .collect::<Result<_>>()?,
        deny: approvals.deny.clone(),
        timeout_secs: None,
        hardline: approvals.hardline,
    })
}
