//! Wire form of the images a message carries.
use crate::{ContentBlock, Message};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// The images of a hydrated message as `{mime_type, data}` values (data is
/// base64), in order. Blocks without bytes were not hydrated and are skipped.
pub fn wire_images(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image {
                image,
                data: Some(data),
            } => Some(json!({"mime_type":image.mime_type,"data":STANDARD.encode(data)})),
            _ => None,
        })
        .collect()
}
