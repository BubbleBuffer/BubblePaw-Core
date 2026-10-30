use super::*;
#[derive(Serialize, Deserialize)]
pub struct SummaryParams {
    pub session_id: String,
    pub kind: crate::context_reads::SummaryKind,
    pub id: String,
}
#[derive(Serialize, Deserialize)]
pub struct RangeParams {
    pub session_id: String,
    #[serde(flatten)]
    pub range: crate::context_reads::HistoryRange,
}
#[derive(Serialize)]
pub struct ViewResult {
    pub view: Option<bubblepaw_core::ContextView>,
}
#[derive(Serialize)]
pub struct EventsResult {
    pub events: Vec<bubblepaw_core::ContextEvent>,
}
pub type StructureResult = crate::context_reads::ContextStructure;
pub type SummaryResult = crate::context_reads::ContextSummary;
pub type RangeResult = crate::context_reads::HistoryRangePage;
pub fn view(ctx: &HostCtx, input: SessionParams) -> Result<ViewResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    Ok(ViewResult {
        view: crate::context_reads::view(&ctx.store, &input.session_id)?,
    })
}
pub fn structure(ctx: &HostCtx, input: SessionParams) -> Result<StructureResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    crate::context_reads::structure(&ctx.store, &input.session_id).map_err(Into::into)
}
pub fn summary(ctx: &HostCtx, input: SummaryParams) -> Result<SummaryResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    crate::context_reads::summary(&ctx.store, &input.session_id, input.kind, &input.id)
        .map_err(Into::into)
}
pub fn events(ctx: &HostCtx, input: SessionParams) -> Result<EventsResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    Ok(EventsResult {
        events: crate::context_reads::events(&ctx.store, &input.session_id)?,
    })
}
pub fn range(ctx: &HostCtx, input: RangeParams) -> Result<RangeResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    crate::context_reads::history_range(&ctx.store, &input.session_id, &input.range)
        .map_err(Into::into)
}
pub(super) fn read(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    match method {
        MethodId::ContextView => encode(view(ctx, decode(params)?)?),
        MethodId::ContextStructure => encode(structure(ctx, decode(params)?)?),
        MethodId::ContextSummary => encode(summary(ctx, decode(params)?)?),
        MethodId::ContextEvents => encode(events(ctx, decode(params)?)?),
        MethodId::HistoryRange => encode(range(ctx, decode(params)?)?),
        _ => unreachable!("context method table"),
    }
}
