use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
#[derive(Serialize, Deserialize)]
pub struct FileData {
    pub file: crate::input_files::InputFileRef,
    pub data: String,
}

pub fn add(
    ctx: &HostCtx,
    input: AddFileParams,
) -> Result<crate::input_files::InputFileRef, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let files = required(&ctx.input_files, "Input attachments")?;
    let max = files.limits().max_file_bytes;
    if input.data.len() as u64 > (max.saturating_add(2) / 3).saturating_mul(4) {
        return Err(HostError::new(
            413,
            "Attachment exceeds the configured file limit",
        ));
    }
    let bytes = STANDARD
        .decode(input.data)
        .map_err(|e| HostError::invalid(e.to_string()))?;
    if bytes.len() as u64 > max {
        return Err(HostError::new(
            413,
            "Attachment exceeds the configured file limit",
        ));
    }
    files
        .upload(&input.session_id, &input.name, &bytes)
        .map_err(|e| HostError::invalid(e.to_string()))
}
pub fn read(ctx: &HostCtx, input: ReadFileParams) -> Result<FileData, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    let files = required(&ctx.input_files, "Input attachments")?;
    let (file, bytes) = files
        .read(&input.session_id, &input.id)
        .map_err(|e| HostError::new(404, e.to_string()))?;
    Ok(FileData {
        file,
        data: STANDARD.encode(bytes),
    })
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    if method == MethodId::AttachmentsAdd {
        encode(add(ctx, decode(params)?)?)
    } else {
        encode(read(ctx, decode(params)?)?)
    }
}
