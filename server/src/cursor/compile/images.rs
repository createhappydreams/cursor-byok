//! Resolves and persists images and blobs referenced by Cursor inputs.
use crate::{
    cursor::{protocol::proto::agent::v1 as pb, services::blob_sync::BlobSynchronizer},
    model::ContentPart,
    store::BlobId,
    Error, Result,
};

pub async fn parts(
    message: &pb::UserMessage,
    text: String,
    blobs: &BlobSynchronizer,
) -> Result<Vec<ContentPart>> {
    let mut parts = vec![ContentPart::Text { text }];
    if let Some(context) = &message.selected_context {
        for image in &context.selected_images {
            // Local patch: a single unresolvable attachment must not fail the whole
            // run. Historical images kept by the Cursor client are frequently
            // unavailable after cache cleanup, which previously aborted the
            // request and left the IDE retrying forever. Degrade to a text
            // placeholder instead and keep the conversation usable.
            let resolved = match image_mime_type(image) {
                Ok(mime_type) => image_data(image, blobs).await.map(|data| (mime_type, data)),
                Err(error) => Err(error),
            };
            match resolved {
                Ok((mime_type, data)) => parts.push(ContentPart::Image { mime_type, data }),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "selected image could not be resolved; continuing with a text placeholder"
                    );
                    parts.push(ContentPart::Text {
                        text: "[image unavailable: attachment data could not be fetched from the Cursor client]"
                            .into(),
                    });
                }
            }
        }
    }
    Ok(parts)
}

fn image_mime_type(image: &pb::SelectedImage) -> Result<String> {
    let mime_type = image.mime_type.trim();
    if !mime_type.starts_with("image/") || mime_type.len() == "image/".len() {
        return Err(Error::Protocol(format!(
            "selected image has invalid MIME type: {}",
            image.mime_type
        )));
    }
    Ok(mime_type.into())
}

async fn image_data(image: &pb::SelectedImage, blobs: &BlobSynchronizer) -> Result<Vec<u8>> {
    use pb::selected_image::DataOrBlobId;

    let data = match image.data_or_blob_id.as_ref() {
        Some(DataOrBlobId::Data(data)) => data.clone(),
        Some(DataOrBlobId::BlobId(raw_id)) => {
            let id = BlobId::from_bytes(raw_id)?;
            blobs.get(&id).await?.ok_or_else(|| {
                Error::Protocol(format!(
                    "selected image Blob is missing: {}",
                    id.to_base64()
                ))
            })?
        }
        Some(DataOrBlobId::BlobIdWithData(value)) => {
            let id = BlobId::from_bytes(&value.blob_id)?;
            if value.data.is_empty() {
                blobs.get(&id).await?.ok_or_else(|| {
                    Error::Protocol(format!(
                        "selected image Blob is missing: {}",
                        id.to_base64()
                    ))
                })?
            } else {
                blobs.cache_received(&id, &value.data).await?;
                value.data.clone()
            }
        }
        None => {
            return Err(Error::Protocol(
                "selected image is missing data_or_blob_id".into(),
            ))
        }
    };
    if data.is_empty() {
        return Err(Error::Protocol("selected image data is empty".into()));
    }
    Ok(data)
}
